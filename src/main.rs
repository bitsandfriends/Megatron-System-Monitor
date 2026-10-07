// SPDX-License-Identifier: GPL-3.0-only
//! COSMIC panel applet showing CPU load, RAM, GPU VRAM and power draw.
//!
//! The panel shows the current values next to matching symbolic icons; a click
//! opens a popup with the exact figures, the curve of the last hour and a
//! collapsible section for the Spark vLLM pair (`src/spark.rs`).

mod graph;
mod install;
mod procs;
mod sensors;
mod spark;

use std::time::Duration;

use cosmic::app::{Core, Task};
use cosmic::iced::window::Id;
use cosmic::iced::advanced::text::Wrapping;
use cosmic::iced::{Alignment, Color, Length, Limits, Subscription};
use cosmic::widget::{Canvas, Column, Row, button, container, divider, icon, scrollable, space, text, text_input, toggler};
use cosmic::{Application, Element};

use crate::graph::{Chart, Series, stats};
use crate::install::{
    AgentVersionState, InstallRequest, InstallState, InstallStatus, Installer,
};
use crate::procs::{LocalProcesses, ProcessList, ProcessTask, SortKey};
use crate::sensors::Monitor;
use crate::spark::{SparkConfig, SparkMonitor, SparkSnapshot};

const APP_ID: &str = "com.iambarth.CosmicAppletSysmon";

const ICON_CPU: &str = "com.iambarth.CosmicAppletSysmon-cpu-symbolic";
const ICON_RAM: &str = "com.iambarth.CosmicAppletSysmon-ram-symbolic";
const ICON_GPU: &str = "com.iambarth.CosmicAppletSysmon-gpu-symbolic";
const ICON_VRAM: &str = "com.iambarth.CosmicAppletSysmon-vram-symbolic";
const ICON_POWER: &str = "com.iambarth.CosmicAppletSysmon-power-symbolic";
const ICON_SPARK: &str = "network-server-symbolic";
const ICON_MORE: &str = "view-more-symbolic";
const ICON_ADD: &str = "list-add-symbolic";
const ICON_UPDATE: &str = "software-update-available-symbolic";
const ICON_REMOVE: &str = "list-remove-symbolic";
const ICON_BACK: &str = "go-previous-symbolic";
const ICON_SAVE: &str = "document-save-symbolic";
const ICON_CHEVRON_OPEN: &str = "go-down-symbolic";
const ICON_CHEVRON_CLOSED: &str = "go-next-symbolic";
const ICON_PROCS: &str = "utilities-system-monitor-symbolic";

const COLOR_CPU: Color = Color::from_rgb(0.87, 0.24, 0.26);
const COLOR_RAM: Color = Color::from_rgb(0.21, 0.52, 0.89);
const COLOR_VRAM: Color = Color::from_rgb(0.62, 0.31, 0.72);
const COLOR_SYSTEM: Color = Color::from_rgb(0.96, 0.57, 0.12);
const COLOR_GPU: Color = Color::from_rgb(0.19, 0.76, 0.50);
/// Curve colours for the individual GPUs (chips are indexed by detection order).
const COLOR_GPU_CHIPS: [Color; 4] = [
    COLOR_GPU,
    Color::from_rgb(0.29, 0.69, 0.94),
    Color::from_rgb(0.96, 0.57, 0.12),
    Color::from_rgb(0.62, 0.31, 0.72),
];
const COLOR_TOKENS: Color = Color::from_rgb(0.29, 0.69, 0.94);
const COLOR_PREFILL: Color = Color::from_rgb(0.55, 0.45, 0.85);
const COLOR_OK: Color = Color::from_rgb(0.19, 0.76, 0.50);
const COLOR_WARN: Color = Color::from_rgb(0.96, 0.57, 0.12);
const COLOR_BAD: Color = Color::from_rgb(0.87, 0.24, 0.26);

const CHART_HEIGHT: f32 = 96.0;
/// Window width of the popup.
///
/// Wide enough for a card row (mark, name, load, temperature, power, VRAM and
/// percent) on one line, narrow enough that a label/value row with its halves and
/// the rule between them still fits without being squeezed.
/// Window width of the popup, computed from the content it has to show: the
/// widest card name (or a sensible default) plus the fixed numeric columns.
/// Never below `POPUP_MIN_WIDTH`, never above `POPUP_MAX_WIDTH`.
const POPUP_MAX_WIDTH: f32 = 900.0;

/// Width the popup needs for `longest_name` characters in the name column.
fn popup_width_for(longest_name: usize) -> f32 {
    // A name column narrower than 24 characters is not usable, wider than 44 is
    // wasted space; the window follows that range.
    let name = text_width(longest_name.clamp(24, 44));
    let content = 32.0
        + MARK_WIDTH
        + name
        + GPU_COL_LOAD
        + GPU_COL_TEMP
        + GPU_COL_POWER
        + GPU_COL_VRAM
        + GPU_COL_VRAM_PCT
        + 6.0 * TABLE_GAP as f32
        + 5.0 * (TABLE_GAP as f32 + 1.0);
    content.clamp(POPUP_MIN_WIDTH, POPUP_MAX_WIDTH)
}

// ---- table look -----------------------------------------------------------
//
// Every list in the popup (values, graphics cards, remote nodes) is drawn on the
// same raster: one label column on the left, fixed-width monospace columns for
// the numbers, a coloured mark that ties a card to its curve, and hairline
// separators between the blocks.
/// Horizontal space between two table columns.
const TABLE_GAP: u16 = 8;
/// Vertical space between two table rows.
const TABLE_ROW_GAP: u16 = 3;
/// Vertical space between the header row and the first data row.
const TABLE_HEAD_GAP: u16 = 5;
/// Thin rule above a total row inside a table.
const TABLE_RULE_WIDTH: f32 = 1.0;
/// Plain, lowercase styling for table headers.
const HEADER_COLOR: Color = Color::from_rgb(0.55, 0.58, 0.64);
/// Width of that mark.
const MARK_WIDTH: f32 = 3.0;

#[derive(Debug, Clone)]
pub enum Message {
    /// One second elapsed: sample every value and extend the history.
    Tick,
    /// The panel button was pressed.
    TogglePopup,
    /// The popup asked to be closed.
    PopupClosed(Id),
    /// The Spark section header was pressed.
    ToggleSpark,
    /// The settings button in the popup header was pressed.
    ToggleSettings,
    /// The host field in the settings view changed.
    HostDraft(String),
    /// Add the typed host to the node list.
    AddHost,
    /// Remove the node at this position.
    RemoveHost(usize),
    /// Switch between discovered engines and explicit data only.
    ToggleDiscovered,
    /// Share one snapshot between several panel instances.
    ToggleSharedCache,
    /// Show or hide the local history charts.
    ToggleLocalCharts,
    /// Choose whether the Spark section starts expanded.
    ToggleSectionOpen,
    /// Open the installer dialog for the typed host.
    OpenInstaller,
    /// Installer dialog fields.
    InstallHost(String),
    InstallUser(String),
    InstallPassword(String),
    InstallPort(String),
    /// Run the installation on the entered host.
    StartInstall,
    /// Add the entered host without installing anything.
    AddHostOnly,
    /// Close the installer dialog.
    CloseInstaller,
    /// Abort a running installation.
    CancelInstall,
    /// Offer/run an agent update for a configured target.
    UpdateAgent(String),
    /// Expand or collapse one remote node.
    ToggleRemote(String),
    /// Open the model list of one engine (`host:8787`, port).
    OpenModels(String, u16),
    /// Load one model into the engine.
    LoadModel(String),
    /// Close the model dialog.
    CloseModels,
    /// Persist the configuration and restart the agent connections.
    SaveSettings,
    /// Open the process page.
    OpenProcesses,
    /// Return from the process page to the values.
    CloseProcesses,
    /// The process search field changed.
    ProcessQuery(String),
    /// Sort the process list by this column.
    ProcessSort(SortKey),
    /// Show the process list of one host (0 = this machine).
    ProcessHost(usize),
    /// Ask for the process list again (node agents only).
    RefreshProcesses,
    /// Expand or collapse the command line of one process.
    ToggleProcess(u32),
    /// Ask for confirmation before sending a signal.
    AskSignal(u32, i32),
    /// Close the confirmation without sending anything.
    CancelSignal,
    /// Send the signal that was confirmed.
    ConfirmSignal,
}

pub struct Sysmon {
    core: Core,
    monitor: Monitor,
    popup: Option<Id>,
    panel_size: Option<cosmic::iced::Size>,
    spark: SparkMonitor,
    spark_state: SparkSnapshot,
    spark_open: bool,
    config: SparkConfig,
    settings_open: bool,
    host_draft: String,
    note: Option<String>,
    installer: Option<Installer>,
    install_state: InstallState,
    install_host: String,
    install_user: String,
    install_password: String,
    install_port: String,
    install_handled: bool,
    /// The dialog was opened to update an existing node.
    install_is_update: bool,
    /// Remote nodes whose details are expanded.
    expanded_remote: Vec<String>,
    /// Name of this machine for the LOKAL heading.
    hostname: String,
    /// Engine whose model list is open: (agent label, port).
    models_open: Option<(String, u16)>,
    /// Running model load.
    loader: Option<crate::spark::ModelLoader>,
    load_state: crate::spark::LoadState,
    /// Whether the process page is shown.
    procs_open: bool,
    /// Search text of the process page.
    proc_query: String,
    /// Column the process list is sorted by.
    proc_sort: SortKey,
    /// Selected host: 0 is this machine, 1.. maps into `config.targets`.
    proc_host: usize,
    /// Local process scan, sampled in a background thread while the page is open.
    proc_local: LocalProcesses,
    /// Last process list of the selected node agent.
    proc_remote: Option<ProcessList>,
    /// Request to a node agent that is still running.
    proc_task: Option<ProcessTask>,
    /// Process and signal waiting for confirmation.
    proc_confirm: Option<(u32, i32)>,
    /// Process whose command line is expanded.
    proc_expanded: Option<u32>,
    /// Result of the last signal or node request.
    proc_note: Option<Result<String, String>>,
}

impl Application for Sysmon {
    type Executor = cosmic::executor::Default;
    type Flags = ();
    type Message = Message;

    const APP_ID: &'static str = APP_ID;

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn init(core: Core, _flags: Self::Flags) -> (Self, Task<Self::Message>) {
        tracing::info!("{APP_ID}: starting");
        let mut monitor = Monitor::new();
        // Prime the delta based counters so the first visible value is real.
        monitor.tick();
        monitor.history.clear();

        // The Spark agents are polled by background threads; the UI only reads
        // the latest snapshot, so no network call ever runs on the UI thread.
        let config = SparkConfig::load();
        let spark_open = config.spark_section_open;
        let spark = SparkMonitor::start(config.clone());
        let spark_state = spark.snapshot();

        // Developer aids: `SYSMON_OPEN_POPUP=1` opens the popup shortly after
        // startup and `SYSMON_OPEN_SETTINGS=1` shows the settings page, which
        // makes both views testable without a pointer click.
        let settings_open = std::env::var_os("SYSMON_OPEN_SETTINGS").is_some();
        tracing::info!("popup settings view on start: {settings_open}");

        // Developer aid: `SYSMON_OPEN_POPUP=1` opens the popup shortly after
        // startup, which makes the popup testable without a pointer click.
        let task = if std::env::var_os("SYSMON_OPEN_POPUP").is_some() {
            Task::perform(
                async {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                },
                |_| cosmic::Action::App(Message::TogglePopup),
            )
        } else {
            Task::none()
        };

        // Development switches for the process page: open it directly, preselect
        // a host, prefill the search, show the confirmation dialog, or send one
        // signal. The last one proves the whole path to the kernel without a
        // pointer click, exactly like `SYSMON_START_INSTALL`.
        let procs_open = std::env::var_os("SYSMON_OPEN_PROCS").is_some();
        let proc_local = LocalProcesses::start();
        proc_local.set_active(procs_open);

        let mut app = Self {
            core,
            monitor,
            popup: None,
            panel_size: None,
            spark,
            spark_state,
            spark_open,
            config,
            settings_open,
            host_draft: String::new(),
            note: None,
            installer: None,
            install_state: InstallState::default(),
            install_host: std::env::var("SYSMON_OPEN_INSTALL")
                .ok()
                .unwrap_or_default(),
            install_user: std::env::var("USER").unwrap_or_else(|_| "root".to_owned()),
            install_password: String::new(),
            install_port: "22".to_owned(),
            install_handled: false,
            install_is_update: false,
            // Development switch: SYSMON_EXPAND_REMOTE=node-01,desktop-01
            expanded_remote: std::env::var("SYSMON_EXPAND_REMOTE")
                .map(|value| {
                    value
                        .split(',')
                        .map(|item| item.trim().to_owned())
                        .filter(|item| !item.is_empty())
                        .collect()
                })
                .unwrap_or_default(),
            hostname: local_hostname(),
            // Development switch: SYSMON_OPEN_MODELS=desktop-01:8787:11434
            models_open: std::env::var("SYSMON_OPEN_MODELS")
                .ok()
                .and_then(|value| {
                    let mut parts = value.rsplitn(2, ':');
                    let port = parts.next().and_then(|item| item.parse::<u16>().ok())?;
                    let target = parts.next()?.to_owned();
                    Some((target, port))
                }),
            loader: None,
            load_state: crate::spark::LoadState::default(),
            procs_open,
            // Development switch: SYSMON_PROC_QUERY=chrome
            proc_query: std::env::var("SYSMON_PROC_QUERY").unwrap_or_default(),
            proc_sort: SortKey::Cpu,
            // Development switch: SYSMON_PROC_HOST=2 (1..len of the node list)
            proc_host: std::env::var("SYSMON_PROC_HOST")
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(0),
            proc_local,
            proc_remote: None,
            proc_task: None,
            // Development switch: SYSMON_PROC_CONFIRM=1234:15
            proc_confirm: std::env::var("SYSMON_PROC_CONFIRM")
                .ok()
                .and_then(|value| parse_pid_signal(&value)),
            proc_expanded: None,
            proc_note: None,
        };

        if procs_open {
            app.refresh_processes();
        }
        // Development switch: SYSMON_PROC_KILL=1234:15 sends one signal through
        // the real handler, local or on the selected node.
        if let Some((pid, signal)) = std::env::var("SYSMON_PROC_KILL")
            .ok()
            .and_then(|value| parse_pid_signal(&value))
        {
            app.perform_signal(pid, signal);
        }

        (app, task)
    }

    fn style(&self) -> Option<cosmic::iced::theme::Style> {
        Some(cosmic::applet::style())
    }

    fn subscription(&self) -> Subscription<Self::Message> {
        cosmic::iced::time::every(Duration::from_secs(1)).map(|_| Message::Tick)
    }

    fn on_close_requested(&self, id: Id) -> Option<Message> {
        Some(Message::PopupClosed(id))
    }

    fn on_window_resize(&mut self, id: Id, width: f32, height: f32) {
        if self.core.main_window_id() == Some(id) {
            self.panel_size = Some(cosmic::iced::Size::new(width, height));
        }
    }

    fn update(&mut self, message: Self::Message) -> Task<Self::Message> {
        match message {
            Message::Tick => {
                self.monitor.tick();
                self.spark_state = self.spark.snapshot();
                self.poll_installer();
                self.poll_process_task();
                if let Some(loader) = &self.loader {
                    self.load_state = loader.snapshot();
                }
                // Development switch: reopen the popup if it was closed, so a
                // screenshot can be taken on a busy desktop.
                if self.popup.is_none() && std::env::var("SYSMON_KEEP_POPUP").is_ok() {
                    return self.update(Message::TogglePopup);
                }
                Task::none()
            }

            Message::ToggleSpark => {
                self.spark_open = !self.spark_open;
                Task::none()
            }

            Message::ToggleSettings => {
                self.settings_open = !self.settings_open;
                self.note = None;
                Task::none()
            }

            Message::HostDraft(value) => {
                self.host_draft = value;
                Task::none()
            }

            Message::AddHost => {
                let host = self.host_draft.trim().to_owned();
                if !host.is_empty()
                {
                    if self.config.add_target(&host) {
                        self.host_draft.clear();
                        let _ = self.config.save();
                    }
                }
                Task::none()
            }

            Message::RemoveHost(index) => {
                if index < self.config.targets.len() {
                    self.config.targets.remove(index);
                }
                Task::none()
            }

            Message::ToggleDiscovered => {
                self.config.use_discovered = !self.config.use_discovered;
                Task::none()
            }

            Message::ToggleSharedCache => {
                self.config.shared_cache = !self.config.shared_cache;
                Task::none()
            }

            Message::ToggleLocalCharts => {
                self.config.show_local_charts = !self.config.show_local_charts;
                Task::none()
            }

            Message::OpenInstaller => {
                self.install_host = if self.host_draft.trim().is_empty() {
                    self.install_host.clone()
                } else {
                    self.host_draft.trim().to_owned()
                };
                self.install_state = InstallState::default();
                self.installer = None;
                self.install_handled = false;
                self.install_is_update = false;
                self.settings_open = false;
                Task::none()
            }

            Message::InstallHost(value) => {
                self.install_host = value;
                Task::none()
            }

            Message::InstallUser(value) => {
                self.install_user = value;
                Task::none()
            }

            Message::InstallPassword(value) => {
                self.install_password = value;
                Task::none()
            }

            Message::InstallPort(value) => {
                self.install_port = value;
                Task::none()
            }

            Message::StartInstall => {
                self.start_install();
                Task::none()
            }

            Message::AddHostOnly => {
                let host = self.install_host.trim().to_owned();
                if self.config.add_target(&host) {
                    self.host_draft.clear();
                    let _ = self.config.save();
                }
                self.installer = None;
                self.install_host.clear();
                self.settings_open = true;
                Task::none()
            }

            Message::OpenModels(target, port) => {
                self.models_open = Some((target, port));
                self.loader = None;
                self.load_state = crate::spark::LoadState::default();
                Task::none()
            }

            Message::LoadModel(model) => {
                if let Some((target, port)) = self.models_open.clone() {
                    self.loader = Some(crate::spark::ModelLoader::start(
                        target,
                        None,
                        port,
                        model,
                    ));
                }
                Task::none()
            }

            Message::CloseModels => {
                self.models_open = None;
                self.loader = None;
                self.load_state = crate::spark::LoadState::default();
                Task::none()
            }

            Message::ToggleRemote(target) => {
                if let Some(index) = self.expanded_remote.iter().position(|item| item == &target) {
                    self.expanded_remote.remove(index);
                } else {
                    self.expanded_remote.push(target);
                }
                Task::none()
            }

            Message::UpdateAgent(target) => {
                let host = target
                    .split(':')
                    .next()
                    .unwrap_or(target.as_str())
                    .to_owned();
                self.install_host = host;
                self.install_is_update = !self.install_host.is_empty();
                self.install_state = InstallState::default();
                self.installer = None;
                self.install_handled = false;
                self.settings_open = false;
                // An agent update replaces the process page, and no scan should
                // keep running behind the installer.
                self.procs_open = false;
                self.proc_local.set_active(false);
                self.proc_task = None;
                Task::none()
            }

            Message::CancelInstall => {
                if let Some(installer) = &self.installer {
                    installer.cancel();
                }
                Task::none()
            }

            Message::CloseInstaller => {
                self.installer = None;
                self.install_state = InstallState::default();
                self.install_host.clear();
                self.host_draft.clear();
                self.install_is_update = false;
                self.settings_open = true;
                Task::none()
            }

            Message::ToggleSectionOpen => {
                self.config.spark_section_open = !self.config.spark_section_open;
                self.spark_open = self.config.spark_section_open;
                Task::none()
            }

            Message::SaveSettings => {
                self.note = Some(match self.config.save() {
                    Ok(path) => {
                        // Reconnect the agents with the saved node list.
                        self.spark = SparkMonitor::start(self.config.clone());
                        self.spark_state = self.spark.snapshot();
                        format!("Gespeichert in {}", path.display())
                    }
                    Err(error) => format!("Speichern fehlgeschlagen: {error}"),
                });
                Task::none()
            }

            Message::OpenProcesses => {
                self.procs_open = true;
                self.proc_note = None;
                self.proc_confirm = None;
                self.proc_local.set_active(true);
                self.refresh_processes();
                Task::none()
            }

            Message::CloseProcesses => {
                self.procs_open = false;
                self.proc_local.set_active(false);
                self.proc_task = None;
                self.proc_remote = None;
                self.proc_confirm = None;
                self.proc_expanded = None;
                Task::none()
            }

            Message::ProcessQuery(value) => {
                self.proc_query = value;
                Task::none()
            }

            Message::ProcessSort(key) => {
                self.proc_sort = key;
                Task::none()
            }

            Message::ProcessHost(index) => {
                if index == self.proc_host {
                    return Task::none();
                }
                self.proc_host = index;
                self.proc_remote = None;
                self.proc_expanded = None;
                self.proc_confirm = None;
                self.proc_note = None;
                self.refresh_processes();
                Task::none()
            }

            Message::RefreshProcesses => {
                self.refresh_processes();
                Task::none()
            }

            Message::ToggleProcess(pid) => {
                self.proc_expanded = if self.proc_expanded == Some(pid) {
                    None
                } else {
                    Some(pid)
                };
                Task::none()
            }

            Message::AskSignal(pid, signal) => {
                self.proc_confirm = Some((pid, signal));
                self.proc_note = None;
                Task::none()
            }

            Message::CancelSignal => {
                self.proc_confirm = None;
                Task::none()
            }

            Message::ConfirmSignal => {
                if let Some((pid, signal)) = self.proc_confirm.take() {
                    self.perform_signal(pid, signal);
                }
                Task::none()
            }

            Message::TogglePopup => {
                if let Some(popup) = self.popup.take() {
                    return cosmic::surface::surface_task(
                        cosmic::surface::action::destroy_popup(popup),
                    );
                }

                cosmic::surface::surface_task(cosmic::surface::action::app_popup(
                    |_| Default::default(),
                    |app: &mut Sysmon| {
                        let new_id = Id::unique();
                        app.popup.replace(new_id);
                        // The process page samples only while it is visible.
                        app.proc_local.set_active(app.procs_open);

                        let mut settings = app.core.applet.get_popup_settings(
                            app.core.main_window_id().expect("panel window"),
                            new_id,
                            Some((1, 1)),
                            None,
                            None,
                        );

                        // `get_popup_settings` anchors to the applet slot; the
                        // window is wider than this applet, so anchor to the
                        // whole panel to keep the popup centred below it.
                        if let Some(size) = app.panel_size {
                            let anchor = &mut settings.positioner.anchor_rect;
                            if app.core.applet.is_horizontal() {
                                anchor.width = anchor.width.max(size.width.round() as i32);
                            } else {
                                anchor.height = anchor.height.max(size.height.round() as i32);
                            }
                        }

                        settings
                    },
                    None,
                ))
            }

            Message::PopupClosed(id) => {
                if self.popup.as_ref() == Some(&id) {
                    self.popup = None;
                    // A closed popup must not keep scanning /proc or asking a node.
                    self.proc_local.set_active(false);
                    self.proc_task = None;
                    self.proc_confirm = None;
                }
                Task::none()
            }
        }
    }

    fn view(&self) -> Element<'_, Self::Message> {
        let sample = &self.monitor.last;
        let spacing = cosmic::theme::spacing();
        let horizontal = self.core.applet.is_horizontal();

        // Fixed field widths keep the panel from twitching when a value drops
        // below ten (10 % -> 9 %): percentages use two digits, watts three.
        let system_watts = if sample.cpu_watts_available {
            format!("{:03.0} W", sample.total_watts())
        } else {
            "--- W".to_owned()
        };

        let items: Vec<Element<'_, Message>> = vec![
            metric(ICON_CPU, format!("{:02.0} %", sample.cpu_load)),
            metric(ICON_RAM, format!("{:02.0} %", sample.mem_pct())),
            metric(ICON_VRAM, format!("{:02.0} %", sample.vram_pct())),
            metric(ICON_POWER, system_watts),
            metric(ICON_GPU, format!("{:03.0} W", sample.gpu_watts)),
        ];

        let content: Element<'_, Message> = if horizontal {
            Row::from_vec(items)
                .align_y(Alignment::Center)
                .spacing(spacing.space_xs)
                .into()
        } else {
            Column::from_vec(items)
                .align_x(Alignment::Center)
                .spacing(spacing.space_xxs)
                .into()
        };

        let padding = self.core.applet.suggested_padding(true);
        let button = button::custom(content)
            .padding(if horizontal {
                [0u16, padding.1]
            } else {
                [padding.0, 0u16]
            })
            .class(cosmic::theme::Button::AppletIcon)
            .on_press(Message::TogglePopup);

        // Applet surfaces start at a fixed placeholder size; `autosize` answers
        // the panel's resize request with the size this content actually needs.
        let mut limits = Limits::NONE.min_width(1.0).min_height(1.0);
        if let Some(bounds) = self.core.applet.suggested_bounds {
            if bounds.width > 0.0 {
                limits = limits.max_width(bounds.width);
            }
            if bounds.height > 0.0 {
                limits = limits.max_height(bounds.height);
            }
        }

        cosmic::widget::autosize::autosize(
            container(button),
            cosmic::widget::Id::new("sysmon-panel"),
        )
        .limits(limits)
        .into()
    }

    fn view_window(&self, _id: Id) -> Element<'_, Self::Message> {
        let sample = &self.monitor.last;
        let spacing = cosmic::theme::spacing();
        let history = &self.monitor.history;

        let cpu_values: Vec<f32> = history.iter().map(|item| item.cpu_load).collect();
        let mem_values: Vec<f32> = history.iter().map(|item| item.mem_pct()).collect();
        let vram_values: Vec<f32> = history.iter().map(|item| item.vram_pct()).collect();
        let cpu_watts_values: Vec<f32> = history.iter().map(|item| item.cpu_watts).collect();
        let gpu_watts_values: Vec<f32> = history.iter().map(|item| item.gpu_watts).collect();

        let samples = history.len();
        let covered = if samples < 60 {
            format!("{samples} s")
        } else {
            format!("{:.0} min", samples as f32 / 60.0)
        };

        // ---- current values -------------------------------------------------
        //
        // Label/value rows keep a fixed value width, so all values end on the
        // same x; the graphics cards get their own table below.
        let mut details = Column::new()
            .spacing(TABLE_ROW_GAP)
            .width(Length::Fill)
            .push(value_row(
                ICON_CPU,
                "CPU-Auslastung",
                &format!("{:.0} %", sample.cpu_load),
                9,
            ))
            .push(value_row(
                ICON_RAM,
                "Arbeitsspeicher",
                &format!(
                    "{:.0}/{:.0} GiB {:.0} %",
                    sample.mem_used,
                    sample.mem_total,
                    sample.mem_pct()
                ),
                9,
            ))
            .push(value_row(
                ICON_POWER,
                "Leistung CPU-Paket",
                &if sample.cpu_watts_available {
                    format!("{:.1} W", sample.cpu_watts)
                } else {
                    "nicht lesbar".to_owned()
                },
                9,
            ))
            .push(value_row(
                ICON_GPU,
                "Leistung GPU",
                &format!("{:.1} W", sample.gpu_watts),
                9,
            ))
            .push(value_row(
                ICON_POWER,
                "Leistung System",
                &if sample.cpu_watts_available {
                    format!("{:.1} W", sample.total_watts())
                } else {
                    format!("{:.1} W (nur GPU)", sample.gpu_watts)
                },
                9,
            ));

        // The graphics card table: one row per card, one total row, exactly the
        // shape btop uses.
        let graphics_block = Column::new()
            .spacing(TABLE_ROW_GAP)
            .width(Length::Fill)
            // Just the title: the explanation only cost a line.
            .push(subsection_heading("GRAFIKKARTEN"))
            .push(local_gpu_table(sample));
        details = details.push(graphics_block);

        // ---- history --------------------------------------------------------
        let load_block = chart_block(
            "Auslastung · letzte Stunde",
            vec![
                Series::new(COLOR_CPU, cpu_values.clone()),
                Series::new(COLOR_RAM, mem_values.clone()),
                Series::new(COLOR_VRAM, vram_values.clone()),
            ],
            100.0,
            vec![
                (COLOR_CPU, "CPU".to_owned()),
                (COLOR_RAM, "RAM".to_owned()),
                (COLOR_VRAM, "VRAM".to_owned()),
            ],
            vec![
                stats_line("CPU", &cpu_values, "%"),
                stats_line("RAM", &mem_values, "%"),
                stats_line("VRAM", &vram_values, "%"),
            ],
        );

        // One curve per card, taken from the history, so a load that switches
        // between the cards stays visible. With a single card the aggregate
        // curve keeps its old label.
        let gpu_count = sample.gpu_count();
        let mut power_series = Vec::new();
        let mut power_legend = Vec::new();
        let mut power_stats = Vec::new();
        for (index, gpu) in sample.gpus.iter().enumerate() {
            let values: Vec<f32> = history
                .iter()
                .map(|item| item.gpus.get(index).map(|gpu| gpu.watts).unwrap_or(0.0))
                .collect();
            let label = if gpu_count > 1 {
                format!("GPU {} · {}", index + 1, gpu.short)
            } else {
                "GPU".to_owned()
            };
            let color = COLOR_GPU_CHIPS[index % COLOR_GPU_CHIPS.len()];
            power_stats.push(stats_line(&label, &values, " W"));
            power_legend.push((color, label));
            power_series.push(Series::new(color, values));
        }
        if gpu_count > 1 {
            // The total is what the panel shows, so it stays in the curve legend.
            power_stats.push(stats_line("GPU gesamt", &gpu_watts_values, " W"));
        }
        if sample.cpu_watts_available {
            power_series.insert(0, Series::new(COLOR_SYSTEM, cpu_watts_values.clone()));
            power_legend.insert(0, (COLOR_SYSTEM, "System".to_owned()));
            power_stats.insert(0, stats_line("System", &cpu_watts_values, " W"));
        }
        let power_max = cpu_watts_values
            .iter()
            .chain(gpu_watts_values.iter())
            .fold(25.0_f32, |acc, value| acc.max(*value));
        let power_max = (power_max / 25.0).ceil() * 25.0;

        let power_block = chart_block(
            "Leistungsaufnahme · letzte Stunde",
            power_series,
            power_max.max(25.0),
            power_legend,
            power_stats,
        );

        // ---- popup ----------------------------------------------------------
        let header = popup_header(samples, &covered);

        let note = if sample.cpu_watts_available {
            "Leistung = RAPL CPU-Paket + alle AMD-GPUs (hwmon). Ein Netzteil-Sensor ist auf diesem System nicht vorhanden."
        } else {
            "CPU-Paket-Leistung nicht lesbar: /sys/class/powercap/intel-rapl:0/energy_uj ist nur für root lesbar. Angezeigt wird nur die GPU-Leistung."
        };

        let content: Element<'_, Message> = if self.procs_open {
            self.processes_view(samples, &covered)
        } else if self.models_open.is_some() {
            self.models_view(samples, &covered)
        } else if self.installer.is_some() || !self.install_host.is_empty() {
            self.installer_view(samples, &covered)
        } else if self.settings_open {
            self.settings_view(samples, &covered)
        } else {
            // Local values first, then one collapsible block per remote node.
            let mut column = Column::new()
                .spacing(spacing.space_s)
                .push(header)
                .push(divider::horizontal::light())
                .push(section_heading("LOKAL", Some(self.hostname.as_str())))
                .push(details);

            // The local charts are optional: without them the popup stays
            // compact and everything fits without scrolling.
            if self.config.show_local_charts {
                column = column
                    .push(load_block)
                    .push(divider::horizontal::light())
                    .push(power_block);
            }

            column
                .push(divider::horizontal::light())
                .push(remote_section(
                    &self.spark_state,
                    self.config.use_discovered,
                    &self.config.client_names,
                    &self.expanded_remote,
                ))
                .push(divider::horizontal::light())
                .push(text::caption(note))
                .into()
        };

        // The popup grows with the collapsible Spark block. A vertical
        // scrollable keeps the window inside the height limit instead of
        // clipping the lower rows. `spacing` embeds the scrollbar so it takes
        // layout space instead of covering the right edge of the values.
        let body = scrollable::vertical(content)
            .spacing(8.0)
            .width(Length::Fill)
            .height(Length::Shrink);

        self.core
            .applet
            .popup_container(container(body).padding(spacing.space_m))
            .limits(
                Limits::NONE
                    .min_width(self.popup_width())
                    .max_width(self.popup_width())
                    .min_height(320.0)
                    .max_height(820.0),
            )
            .into()
    }
}

impl Sysmon {
    /// Window width for the current content: as wide as the longest card name
    /// needs, so the columns always fit and nothing has to be squeezed.
    fn popup_width(&self) -> f32 {
        let local = self
            .monitor
            .last
            .gpus
            .iter()
            .map(|gpu| gpu.name.chars().count())
            .max()
            .unwrap_or(0);
        // Cards of the monitored nodes share the same table, so their names count
        // as well — otherwise a long NVIDIA name would be cut off.
        let remote = self.spark_state.longest_gpu_name();
        popup_width_for(local.max(remote).max(24))
    }

    /// Starts the installation for the host in the dialog.
    fn start_install(&mut self) {
        if self.installer.is_some() && !self.install_state.is_finished() {
            return;
        }
        let request = InstallRequest {
            host: self.install_host.trim().to_owned(),
            user: self.install_user.trim().to_owned(),
            password: std::mem::take(&mut self.install_password),
            port: self.install_port.trim().parse().unwrap_or(22),
        };
        self.installer = Some(Installer::start(request));
        self.install_state = InstallState::default();
        self.install_handled = false;
    }

    /// Mirrors the installer state into the UI and adopts a successful host.
    fn poll_installer(&mut self) {
        // Development switch: SYSMON_START_INSTALL=1 starts the installation of the
        // host given in SYSMON_OPEN_INSTALL without a click.
        if self.installer.is_none()
            && !self.install_host.is_empty()
            && std::env::var("SYSMON_START_INSTALL").is_ok()
        {
            self.start_install();
        }
        let Some(installer) = &self.installer else {
            return;
        };
        self.install_state = installer.snapshot();
        if !self.install_state.is_finished() || self.install_handled {
            return;
        }
        self.install_handled = true;
        if self.install_state.succeeded() {
            let host = self.install_host.clone();
            if self.config.add_target(&host) {
                match self.config.save() {
                    Ok(path) => self.note = Some(format!("Knoten aufgenommen, gespeichert in {}", path.display())),
                    Err(error) => self.note = Some(format!("Speichern fehlgeschlagen: {error}")),
                }
                self.spark = SparkMonitor::start(self.config.clone());
                self.spark_state = self.spark.snapshot();
            } else if !host.trim().is_empty() {
                // Already known: say so instead of adding it a second time.
                self.note = Some(format!("{host} ist bereits in der Knotenliste"));
            }
        }
    }

    /// Starts a process request for the selected host.
    ///
    /// The local machine is scanned by its own thread; a node agent is asked
    /// over one short-lived WebSocket connection, never on the UI thread.
    fn refresh_processes(&mut self) {
        if self.selected_is_local() {
            self.proc_local.set_active(true);
            return;
        }
        let Some(target) = self.config.targets.get(self.proc_host - 1).cloned() else {
            return;
        };
        self.proc_task = Some(ProcessTask::list(target, self.config.token.clone()));
    }

    /// Adopts a finished agent request and refreshes after a signal.
    fn poll_process_task(&mut self) {
        let Some(task) = &self.proc_task else {
            return;
        };
        let state = task.snapshot();
        if state.running {
            return;
        }
        self.proc_task = None;
        if let Some(list) = state.list {
            if let Some(error) = &list.error {
                self.proc_note = Some(Err(error.clone()));
            }
            self.proc_remote = Some(list);
        }
        if let Some(result) = state.kill {
            let delivered = result.is_ok();
            self.proc_note = Some(result);
            if delivered {
                // The next scan must show that the process is gone.
                self.refresh_processes();
            }
        }
    }

    /// Sends the confirmed signal to the selected host.
    fn perform_signal(&mut self, pid: u32, signal: i32) {
        if self.selected_is_local() {
            self.proc_note = Some(crate::procs::kill_local(pid, signal));
            return;
        }
        if let Some(target) = self.config.targets.get(self.proc_host - 1).cloned() {
            self.proc_task = Some(ProcessTask::kill(
                target,
                self.config.token.clone(),
                pid,
                signal,
            ));
        }
    }

    /// Rows of the selected host, including rows a node agent has not sent yet.
    fn current_process_list(&self) -> ProcessList {
        if self.selected_is_local() {
            self.proc_local.snapshot()
        } else {
            self.proc_remote.clone().unwrap_or_default()
        }
    }

    /// True when the selected entry is this machine.
    ///
    /// The first entry is always the local machine. A configured node agent that
    /// reports this hostname is treated the same way: reading `/proc` directly is
    /// faster and keeps the owner names correct, which a sandboxed user service
    /// cannot guarantee inside its user namespace.
    fn selected_is_local(&self) -> bool {
        if self.proc_host == 0 {
            return true;
        }
        match self.config.targets.get(self.proc_host - 1) {
            Some(target) => self
                .spark_state
                .agents
                .iter()
                .any(|agent| &agent.label == target && agent.host == self.hostname),
            None => true,
        }
    }

    /// Configured target of the selected host; `None` is this machine.
    fn selected_target(&self) -> Option<&String> {
        if self.proc_host == 0 {
            None
        } else {
            self.config.targets.get(self.proc_host - 1)
        }
    }

    /// Name of the selected host as the header shows it.
    fn selected_host_label(&self) -> String {
        match self.selected_target() {
            None => format!("{} (lokal)", self.hostname),
            Some(target) => {
                let name = self
                    .spark_state
                    .agents
                    .iter()
                    .find(|agent| &agent.label == target)
                    .map(|agent| agent.display_name())
                    .unwrap_or_else(|| target.clone());
                if self.selected_is_local() {
                    format!("{name} (lokal)")
                } else {
                    name
                }
            }
        }
    }

    /// False while the selected agent is too old for the process actions.
    fn selected_agent_ready(&self) -> bool {
        match self.selected_target() {
            None => true,
            Some(target) => self
                .spark_state
                .agents
                .iter()
                .find(|agent| &agent.label == target)
                .map(|agent| crate::procs::supports_processes(&agent.version))
                .unwrap_or(true),
        }
    }

    fn find_process(&self, pid: u32) -> Option<crate::procs::ProcessEntry> {
        self.current_process_list()
            .entries
            .into_iter()
            .find(|entry| entry.pid == pid)
    }

    /// Process page: host selection, search, sorting and the signal actions.
    fn processes_view(&self, samples: usize, covered: &str) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();
        let list = self.current_process_list();
        let rows = crate::procs::filter_and_sort(&list.entries, &self.proc_query, self.proc_sort);
        let running = self
            .proc_task
            .as_ref()
            .map(|task| task.snapshot().running)
            .unwrap_or(false);

        let mut column = Column::new()
            .spacing(spacing.space_s)
            .push(popup_header(samples, covered))
            .push(divider::horizontal::light())
            .push(
                Row::new()
                    .align_y(Alignment::Center)
                    .spacing(spacing.space_xs)
                    .push(icon_element(ICON_PROCS, 20))
                    .push(text::title4("Prozesse"))
                    .push(
                        container(text::caption(format!(
                            "{} von {} · {} Kerne",
                            rows.len(),
                            list.total,
                            list.cores
                        )))
                        .width(Length::Fill)
                        .align_x(Alignment::End),
                    )
                    .push(
                        button::custom(text::body("Zurück"))
                            .class(cosmic::theme::Button::Text)
                            .on_press(Message::CloseProcesses),
                    ),
            );

        // ---- host selection -------------------------------------------------
        let mut hosts = Row::new()
            .align_y(Alignment::Center)
            .spacing(spacing.space_xxs)
            .push(text::caption("Host").width(Length::Fixed(38.0)))
            .push(host_button(
                truncate_label(&format!("{} (lokal)", self.hostname), 18),
                0,
                self.proc_host,
            ));
        for (index, target) in self.config.targets.iter().enumerate() {
            // A node that reports this hostname is this machine: mark it so the
            // user can see that the local read path is used for it.
            let (name, is_self) = self
                .spark_state
                .agents
                .iter()
                .find(|agent| &agent.label == target)
                .map(|agent| (agent.display_name(), agent.host == self.hostname))
                .unwrap_or_else(|| {
                    (
                        target.split(':').next().unwrap_or(target).to_owned(),
                        false,
                    )
                });
            let label = if is_self {
                format!("{name} (lokal)")
            } else {
                name
            };
            hosts = hosts.push(host_button(
                truncate_label(&label, 18),
                index + 1,
                self.proc_host,
            ));
        }
        hosts = hosts.push(space::horizontal());
        hosts = hosts.push(icon_button(ICON_UPDATE, Message::RefreshProcesses));
        column = column.push(hosts);

        // ---- search and sort ------------------------------------------------
        column = column.push(
            Row::new()
                .align_y(Alignment::Center)
                .spacing(spacing.space_xs)
                .width(Length::Fill)
                .push(
                    text_input::text_input(
                        "Suchen: Name, Befehl, PID oder Benutzer",
                        self.proc_query.as_str(),
                    )
                    .on_input(Message::ProcessQuery),
                ),
        );
        let mut sort_row = Row::new()
            .align_y(Alignment::Center)
            .spacing(spacing.space_xxs)
            .push(text::caption("Sortierung").width(Length::Fixed(74.0)));
        for key in [SortKey::Cpu, SortKey::Memory, SortKey::Pid, SortKey::Name] {
            sort_row = sort_row.push(sort_button(key, self.proc_sort));
        }
        column = column.push(sort_row);

        // ---- confirmation ---------------------------------------------------
        if let Some((pid, signal)) = self.proc_confirm {
            let entry = self.find_process(pid);
            let name = entry
                .as_ref()
                .map(|entry| entry.name.clone())
                .unwrap_or_else(|| "unbekannt".to_owned());
            let cmd = entry
                .as_ref()
                .map(|entry| entry.cmd.clone())
                .unwrap_or_default();
            let signal_name = crate::procs::signal_name(signal);
            let warning = if signal == crate::procs::SIGKILL {
                "SIGKILL beendet den Prozess sofort im Kernel: er kann nicht mehr speichern oder aufräumen."
            } else {
                "SIGTERM fordert den Prozess zum Beenden auf; er kann noch speichern und aufräumen."
            };
            let card = Column::new()
                .spacing(spacing.space_xxs)
                .push(text::heading(format!(
                    "Signal {signal} ({signal_name}) an PID {pid} senden?"
                )))
                .push(text::body(format!(
                    "{name} · {}",
                    self.selected_host_label()
                )))
                .push(text::monotext(truncate_label(&cmd, 90)).size(11.0))
                .push(text::caption(warning))
                .push(
                    Row::new()
                        .spacing(spacing.space_xs)
                        .push(
                            button::custom(text::body("Abbrechen"))
                                .class(cosmic::theme::Button::Text)
                                .on_press(Message::CancelSignal),
                        )
                        .push(
                            button::custom(text::body("Signal senden"))
                                .class(cosmic::theme::Button::Destructive)
                                .on_press(Message::ConfirmSignal),
                        ),
                );
            column = column.push(
                container(card)
                    .padding(spacing.space_s)
                    .class(cosmic::theme::Container::Card),
            );
        }

        // ---- agent hint -----------------------------------------------------
        if !self.selected_agent_ready() {
            if let Some(target) = self.selected_target().cloned() {
                let version = self
                    .spark_state
                    .agents
                    .iter()
                    .find(|agent| agent.label == target)
                    .map(|agent| agent.version.clone())
                    .unwrap_or_default();
                column = column.push(caption_colored(
                    format!(
                        "Der Agent auf {target} meldet Version {version} und kennt die Prozessliste nicht. \
                         Ein Update über die Einstellungen schaltet sie frei."
                    ),
                    COLOR_WARN,
                ));
                column = column.push(
                    Row::new()
                        .spacing(spacing.space_xs)
                        .push(icon_button_suggested(
                            ICON_UPDATE,
                            Message::UpdateAgent(target),
                        ))
                        .push(text::caption("Agent aktualisieren")),
                );
            }
        }

        // ---- table ----------------------------------------------------------
        column = column.push(text::caption(
            "Beenden sendet SIGTERM (der Prozess kann aufräumen), Killen sendet SIGKILL \
             (sofort, ohne Aufräumen). Beide fragen vorher nach.",
        ));
        column = column.push(
            Row::new()
                .align_y(Alignment::Center)
                .spacing(spacing.space_xxs)
                .push(process_cell("PID".to_owned(), 42.0, true, true))
                .push(process_cell("Name".to_owned(), 118.0, false, true))
                .push(process_cell("CPU %".to_owned(), 44.0, true, true))
                .push(process_cell("RAM MiB".to_owned(), 58.0, true, true))
                .push(
                    container(text::caption("Aktion"))
                        .width(Length::Fixed(104.0))
                        .align_x(Alignment::Center),
                ),
        );

        let mut table = Column::new().spacing(2);
        for entry in &rows {
            let expanded = self.proc_expanded == Some(entry.pid);
            let selected = self.proc_confirm.map(|(pid, _)| pid) == Some(entry.pid);
            let class = if selected {
                cosmic::theme::Button::Suggested
            } else {
                cosmic::theme::Button::Text
            };
            let row = Row::new()
                .align_y(Alignment::Center)
                .spacing(spacing.space_xxs)
                .push(process_cell(format!("{}", entry.pid), 42.0, true, false))
                .push(
                    container(
                        button::custom(
                            text::monotext(truncate_label(&entry.name, 15)).size(11.0),
                        )
                        .padding([2u16, 4u16])
                        .class(class)
                        .on_press(Message::ToggleProcess(entry.pid)),
                    )
                    .width(Length::Fixed(118.0)),
                )
                .push(process_cell(
                    format!("{:.1}", entry.cpu),
                    44.0,
                    true,
                    false,
                ))
                .push(process_cell(
                    format!("{:.0}", entry.mem_mib()),
                    58.0,
                    true,
                    false,
                ))
                .push(
                    container(
                        button::custom(text::caption("Ende").size(11.0))
                            .padding([2u16, 5u16])
                            .class(cosmic::theme::Button::Text)
                            .on_press(Message::AskSignal(entry.pid, crate::procs::SIGTERM)),
                    )
                    .width(Length::Fixed(50.0)),
                )
                .push(
                    container(
                        button::custom(text::caption("Kill").size(11.0))
                            .padding([2u16, 5u16])
                            .class(cosmic::theme::Button::Destructive)
                            .on_press(Message::AskSignal(entry.pid, crate::procs::SIGKILL)),
                    )
                    .width(Length::Fixed(48.0)),
                );
            table = table.push(row);
            if expanded {
                table = table.push(
                    container(
                        Column::new()
                            .spacing(1)
                            .push(text::monotext(entry.cmd.clone()).size(10.0))
                            .push(text::caption(format!(
                                "Benutzer {} · Status {} ({}) · PPID {} · RAM {:.0} MiB",
                                entry.user,
                                entry.state,
                                entry.state_label(),
                                entry.ppid,
                                entry.mem_mib()
                            ))),
                    )
                    .padding([0u16, 8u16]),
                );
            }
        }
        if rows.is_empty() {
            table = table.push(text::caption(if list.entries.is_empty() {
                "Keine Prozesse gemeldet."
            } else {
                "Keine Treffer für die Suche."
            }));
        }
        column = column.push(
            scrollable::vertical(table)
                .spacing(6.0)
                .height(Length::Fixed(330.0)),
        );

        // ---- status ---------------------------------------------------------
        if running {
            column = column.push(caption_colored(
                format!("Frage {} ab …", self.selected_host_label()),
                COLOR_WARN,
            ));
        }
        if let Some(error) = &list.error {
            column = column.push(caption_colored(error.clone(), COLOR_BAD));
        }
        if let Some(note) = &self.proc_note {
            let (message, color) = match note {
                Ok(text) => (text.clone(), COLOR_OK),
                Err(text) => (text.clone(), COLOR_BAD),
            };
            column = column.push(caption_colored(message, color));
        }
        column = column.push(text::caption(
            "Lokal liest das Applet /proc, Knoten antworten über ihren Agenten. Signale wirken nur auf \
             Prozesse des eigenen Benutzers; sonst meldet das System „keine Berechtigung (EPERM)\". \
             CPU % bezieht sich auf einen Kern (über 100 % = mehrere Threads), RAM ist der residente Speicher.",
        ));
        column.into()
    }

    /// Dialog with the models of one engine; models can be loaded from here.
    fn models_view(&self, samples: usize, covered: &str) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();
        let Some((target, port)) = self.models_open.clone() else {
            return Column::new().into();
        };
        let agent = self
            .spark_state
            .agents
            .iter()
            .find(|agent| agent.label == target);
        let engine = agent.and_then(|agent| {
            agent
                .engines
                .iter()
                .find(|engine| engine.port == port)
        });
        let name = agent
            .map(|agent| agent.display_name())
            .unwrap_or_else(|| target.clone());

        let mut column = Column::new()
            .spacing(spacing.space_s)
            .push(popup_header(samples, covered))
            .push(divider::horizontal::light())
            .push(
                Row::new()
                    .align_y(Alignment::Center)
                    .spacing(spacing.space_xs)
                    .width(Length::Fill)
                    .push(icon_element(ICON_SPARK, 18))
                    .push(text::title4(format!("Modelle · {name}:{port}")))
                    .push(
                        container(text::caption(format!("{}", engine.map(|e| e.model_count).unwrap_or(0))))
                            .width(Length::Fill)
                            .align_x(Alignment::End),
                    ),
            );

        match engine {
            None => {
                column = column.push(text::caption(
                    "Dieser Endpunkt meldet gerade keine Modelle.",
                ));
            }
            Some(engine) => {
                if engine.kind != "ollama" {
                    column = column.push(text::caption(
                        "Laden ist nur für Ollama-Endpunkte möglich; die Liste zeigt die angebotenen Modelle.",
                    ));
                }
                if !engine.loaded.is_empty() {
                    column = column.push(text::caption(format!(
                        "Geladen: {}",
                        engine.loaded.join(", ")
                    )));
                }
                let mut list = Column::new().spacing(spacing.space_xxs);
                for model in &engine.models {
                    let is_loaded = engine.loaded.iter().any(|item| item == model);
                    let mut row = Row::new()
                        .align_y(Alignment::Center)
                        .spacing(spacing.space_xs)
                        .width(Length::Fill)
                        .push(text::monotext(model.clone()).size(12.0).width(Length::Fill));
                    if is_loaded {
                        row = row.push(caption_colored("geladen".to_owned(), COLOR_OK));
                    }
                    if engine.kind == "ollama" {
                        row = row.push(
                            button::custom(text::body("Laden"))
                                .class(cosmic::theme::Button::Suggested)
                                .on_press(Message::LoadModel(model.clone())),
                        );
                    }
                    list = list.push(row);
                }
                if engine.models.is_empty() {
                    list = list.push(text::caption("keine Modelle gemeldet"));
                }
                column = column.push(
                    scrollable::vertical(list)
                        .spacing(6.0)
                        .height(Length::Fixed(320.0)),
                );
            }
        }

        if self.load_state.running || !self.load_state.message.is_empty() {
            let color = if self.load_state.running {
                COLOR_WARN
            } else if self.load_state.failed {
                COLOR_BAD
            } else {
                COLOR_OK
            };
            let message = if self.load_state.running {
                "Lade …".to_owned()
            } else {
                self.load_state.message.clone()
            };
            column = column.push(caption_colored(message, color));
        }

        column = column.push(
            Row::new()
                .spacing(spacing.space_s)
                .push(
                    button::custom(text::body("Schließen"))
                        .class(cosmic::theme::Button::Text)
                        .on_press(Message::CloseModels),
                ),
        );
        column.into()
    }

    /// Installer dialog: host, credentials, live log and the actions.
    fn installer_view(&self, samples: usize, covered: &str) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();
        let running = self.install_state.is_running();

        let mut column = Column::new()
            .spacing(spacing.space_s)
            .push(popup_header(samples, covered))
            .push(divider::horizontal::light())
            .push(
                Row::new()
                    .align_y(Alignment::Center)
                    .spacing(spacing.space_xs)
                    .push(icon_element(ICON_ADD, 20))
                    .push(text::title4(if self.install_is_update {
                "Agent aktualisieren"
            } else {
                "Agent auf Knoten installieren"
            })),
            )
            .push(text::caption(if self.install_is_update {
                "Aktualisierung: Das Applet überträgt die mitgelieferte Agent-Version \
                 und startet den Dienst neu."
            } else {
                "Der Agent, die systemd-Unit und das Installationsskript sind im Applet \
                 enthalten und werden per SSH übertragen. Für die Einrichtung wird ein \
                 Benutzer mit sudo-Rechten benötigt; ohne Passwort wird der SSH-Schlüssel \
                 beziehungsweise ein passwortloses sudo verwendet."
            }));

        column = column.push(
            Row::new()
                .align_y(Alignment::Center)
                .spacing(spacing.space_xs)
                .width(Length::Fill)
                .push(text::body("Host").width(Length::Fixed(90.0)))
                .push(
                    text_input::text_input("spark-03", self.install_host.as_str())
                        .on_input(Message::InstallHost)
                        .on_submit(|_| Message::StartInstall),
                ),
        );
        column = column.push(
            Row::new()
                .align_y(Alignment::Center)
                .spacing(spacing.space_xs)
                .width(Length::Fill)
                .push(text::body("Benutzer").width(Length::Fixed(90.0)))
                .push(
                    text_input::text_input("sysmon", self.install_user.as_str())
                        .on_input(Message::InstallUser),
                ),
        );
        column = column.push(
            Row::new()
                .align_y(Alignment::Center)
                .spacing(spacing.space_xs)
                .width(Length::Fill)
                .push(text::body("Passwort").width(Length::Fixed(90.0)))
                .push(
                    text_input::secure_input(
                        "leer lassen für SSH-Schlüssel",
                        self.install_password.as_str(),
                        None,
                        true,
                    )
                    .on_input(Message::InstallPassword),
                ),
        );
        column = column.push(
            Row::new()
                .align_y(Alignment::Center)
                .spacing(spacing.space_xs)
                .width(Length::Fill)
                .push(text::body("SSH-Port").width(Length::Fixed(90.0)))
                .push(
                    text_input::text_input("22", self.install_port.as_str())
                        .on_input(Message::InstallPort),
                ),
        );

        let mut actions = Row::new().spacing(spacing.space_s);
        if running {
            actions = actions.push(text::caption("Installation läuft …"));
            actions = actions.push(
                button::custom(text::body("Abbrechen"))
                    .class(cosmic::theme::Button::Text)
                    .on_press(Message::CancelInstall),
            );
        } else {
            actions = actions.push(
                button::custom(
                    Row::new()
                        .align_y(Alignment::Center)
                        .spacing(spacing.space_xxs)
                        .push(icon_element(ICON_SAVE, 16))
                        .push(text::body(if self.install_state.is_finished() {
                            "Erneut versuchen"
                        } else {
                            "Installieren und verbinden"
                        })),
                )
                .class(cosmic::theme::Button::Suggested)
                .on_press(Message::StartInstall),
            );
        }
        actions = actions.push(
            button::custom(text::body("Nur hinzufügen ohne Installation"))
                .class(cosmic::theme::Button::Text)
                .on_press(Message::AddHostOnly),
        );
        actions = actions.push(
            button::custom(text::body("Schließen"))
                .class(cosmic::theme::Button::Text)
                .on_press(Message::CloseInstaller),
        );
        column = column.push(actions);

        column = column.push(divider::horizontal::light());
        column = column.push(
            Row::new()
                .align_y(Alignment::Center)
                .spacing(spacing.space_xs)
                .push(text::heading("Protokoll"))
                .push(container(
                    caption_colored(
                        match self.install_state.status {
                            InstallStatus::Idle => "bereit".to_owned(),
                            InstallStatus::Running => "läuft".to_owned(),
                            InstallStatus::Done => "erfolgreich".to_owned(),
                            InstallStatus::Failed => "fehlgeschlagen".to_owned(),
                        },
                        match self.install_state.status {
                            InstallStatus::Done => COLOR_OK,
                            InstallStatus::Failed => COLOR_BAD,
                            InstallStatus::Running => COLOR_WARN,
                            InstallStatus::Idle => COLOR_RAM,
                        },
                    ),
                )
                .width(Length::Fill)
                .align_x(Alignment::End)),
        );

        let mut log_column = Column::new().spacing(1);
        for line in self.install_state.tail(200) {
            log_column = log_column.push(
                text::monotext(line)
                    .size(11.0)
                    .line_height(cosmic::iced::widget::text::LineHeight::Absolute(14.0.into())),
            );
        }
        if self.install_state.lines.is_empty() {
            log_column = log_column.push(text::caption("Noch keine Ausgabe."));
        }
        column = column.push(scrollable::vertical(log_column).spacing(8.0).height(Length::Fixed(260.0)));

        column = column.push(text::caption(format!(
            "Der Knoten wird nach erfolgreicher Installation als {}:8787 aufgenommen.",
            self.install_host
        )));
        column.into()
    }

    /// Settings page: node agents, engine discovery and persistence.
    fn settings_view(&self, samples: usize, covered: &str) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();
        let mut column = Column::new()
            .spacing(spacing.space_s)
            .push(popup_header(samples, covered))
            .push(divider::horizontal::light())
            .push(
                Row::new()
                    .align_y(Alignment::Center)
                    .spacing(spacing.space_xs)
                    .push(icon_element(ICON_MORE, 20))
                    .push(text::title4("Einstellungen")),
            )
            .push(text::caption(
                "Der Agent auf jedem Knoten sucht LLM-Server selbst: er prüft die üblichen \
                 Ports und meldet gefundene Engines samt Modellen. Hier genügt der Hostname.",
            ));

        column = column.push(
            Row::new()
                .align_y(Alignment::Center)
                .width(Length::Fill)
                .spacing(spacing.space_xs)
                .push(text::body("Erkannte Engines verwenden").width(Length::Fill))
                .push(
                    toggler(self.config.use_discovered)
                        .on_toggle(|_| Message::ToggleDiscovered),
                ),
        );

        column = column.push(
            Row::new()
                .align_y(Alignment::Center)
                .width(Length::Fill)
                .spacing(spacing.space_xs)
                .push(text::body("Nur einmal abfragen (mehrere Ausgaben)").width(Length::Fill))
                .push(
                    toggler(self.config.shared_cache)
                        .on_toggle(|_| Message::ToggleSharedCache),
                ),
        );
        column = column.push(text::caption(
            "An: alle Panel-Instanzen teilen einen Abruf über einen Cache im \
             Laufzeitverzeichnis. Aus: jede Instanz verbindet sich selbst.",
        ));

        column = column.push(
            Row::new()
                .align_y(Alignment::Center)
                .width(Length::Fill)
                .spacing(spacing.space_xs)
                .push(text::body("Spark-Sektion beim Öffnen aufklappen").width(Length::Fill))
                .push(
                    toggler(self.config.spark_section_open)
                        .on_toggle(|_| Message::ToggleSectionOpen),
                ),
        );
        column = column.push(
            Row::new()
                .align_y(Alignment::Center)
                .width(Length::Fill)
                .spacing(spacing.space_xs)
                .push(text::body("Lokale Verlaufskurven anzeigen").width(Length::Fill))
                .push(
                    toggler(self.config.show_local_charts)
                        .on_toggle(|_| Message::ToggleLocalCharts),
                ),
        );
        column = column.push(text::caption(
            "Aus: das Popup bleibt kompakt und zeigt nur Werte, Spark-Sektion und \
             die LLM-Kurven.",
        ));

        column = column.push(divider::horizontal::light());
        column = column.push(text::heading("Knoten-Agenten"));
        if self.config.targets.is_empty() {
            column = column.push(text::caption("Kein Knoten konfiguriert."));
        }
        let mismatches = self.spark_state.version_mismatches();
        for (index, target) in self.config.targets.iter().enumerate() {
            let state = self
                .spark_state
                .agents
                .iter()
                .find(|agent| &agent.label == target);
            let (version_text, color) = match state {
                Some(agent) if !agent.online => ("offline".to_owned(), COLOR_RAM),
                Some(agent) if agent.version.is_empty() => ("Version unbekannt".to_owned(), COLOR_WARN),
                Some(agent) => {
                    match crate::install::agent_version_state(&agent.version) {
                        AgentVersionState::Current => (
                            format!("Agent {} · aktuell", agent.version),
                            COLOR_OK,
                        ),
                        AgentVersionState::Outdated => (
                            format!(
                                "Agent {} · Update auf {} verfügbar",
                                agent.version,
                                crate::install::expected_agent_version()
                            ),
                            COLOR_WARN,
                        ),
                        AgentVersionState::AppletOutdated => (
                            format!("Agent {} · neuer als das Applet", agent.version),
                            COLOR_WARN,
                        ),
                        AgentVersionState::Unknown => ("Version unbekannt".to_owned(), COLOR_WARN),
                    }
                }
                None => ("nicht konfiguriert".to_owned(), COLOR_RAM),
            };
            let needs_update = state
                .map(|agent| {
                    crate::install::agent_version_state(&agent.version)
                        == AgentVersionState::Outdated
                })
                .unwrap_or(false);

            let mut row = Row::new()
                .align_y(Alignment::Center)
                .spacing(spacing.space_xs)
                .width(Length::Fill)
                .push(icon_element(ICON_SPARK, 16))
                .push(text::monotext(target.clone()).width(Length::Fill))
                .push(caption_colored(version_text, color));
            if needs_update {
                row = row.push(icon_button_suggested(
                    ICON_UPDATE,
                    Message::UpdateAgent(target.clone()),
                ));
            }
            row = row.push(icon_button(ICON_REMOVE, Message::RemoveHost(index)));
            column = column.push(row);
        }

        if !mismatches.is_empty() {
            column = column.push(text::caption(format!(
                "{} Knoten laufen mit einer anderen Agent-Version als das Applet (erwartet {}).",
                mismatches.len(),
                crate::install::expected_agent_version()
            )));
        }
        column = column.push(
            Row::new()
                .align_y(Alignment::Center)
                .spacing(spacing.space_xs)
                .width(Length::Fill)
                .push(
                    text_input::text_input("node-01 oder host:8787", self.host_draft.as_str())
                        .on_input(Message::HostDraft)
                        .on_submit(|_| Message::AddHost),
                )
                .push(icon_button(ICON_ADD, Message::OpenInstaller)),
        );
        column = column.push(text::caption(format!(
            "Port 8787 wird ergänzt, wenn keiner angegeben ist ({} Knoten konfiguriert).",
            self.config.targets.len()
        )));

        let engines = self.spark_state.engine_details();
        if !engines.is_empty() {
            column = column.push(divider::horizontal::light());
            column = column.push(text::heading("Vom Agenten erkannt"));
            for line in engines {
                column = column.push(text::caption(format!("↳ {line}")));
            }
        }

        if let Some(note) = &self.note {
            let color = if note.contains("fehlgeschlagen") { COLOR_BAD } else { COLOR_OK };
            column = column.push(caption_colored(note.clone(), color));
        }

        column = column.push(divider::horizontal::light());
        column = column.push(
            Row::new()
                .spacing(spacing.space_s)
                .push(
                    button::custom(
                        Row::new()
                            .align_y(Alignment::Center)
                            .spacing(spacing.space_xxs)
                            .push(icon_element(ICON_SAVE, 16))
                            .push(text::body("Speichern und neu verbinden")),
                    )
                    .class(cosmic::theme::Button::Suggested)
                    .on_press(Message::SaveSettings),
                )
                .push(
                    button::custom(
                        Row::new()
                            .align_y(Alignment::Center)
                            .spacing(spacing.space_xxs)
                            .push(icon_element(ICON_BACK, 16))
                            .push(text::body("Zurück")),
                    )
                    .class(cosmic::theme::Button::Text)
                    .on_press(Message::ToggleSettings),
                ),
        );
        column.into()
    }
}

/// Header of the popup with the settings button.
fn popup_header(samples: usize, covered: &str) -> Element<'static, Message> {
    let spacing = cosmic::theme::spacing();
    Row::new()
        .align_y(Alignment::Center)
        .spacing(spacing.space_xs)
        .push(icon_element(ICON_POWER, 24))
        .push(text::title4("System-Monitor"))
        .push(
            container(text::caption(format!("{samples} Werte · {covered} · 1 s Takt")))
                .width(Length::Fill)
                .align_x(Alignment::End),
        )
        .push(icon_button(ICON_PROCS, Message::OpenProcesses))
        .push(icon_button(ICON_MORE, Message::ToggleSettings))
        .into()
}

/// Icon-only button that stands out (used for the agent update action).
fn icon_button_suggested(name: &'static str, message: Message) -> Element<'static, Message> {
    button::custom(icon_element(name, 16))
        .padding([2u16, 6u16])
        .class(cosmic::theme::Button::Suggested)
        .on_press(message)
        .into()
}

/// A small icon-only button used for the toolbar actions.
fn icon_button(name: &'static str, message: Message) -> Element<'static, Message> {
    button::custom(icon_element(name, 16))
        .padding([2u16, 6u16])
        .class(cosmic::theme::Button::Text)
        .on_press(message)
        .into()
}

/// Parses `PID:SIGNAL`, used by the development switches of the process page.
fn parse_pid_signal(value: &str) -> Option<(u32, i32)> {
    let (pid, signal) = value.split_once(':')?;
    Some((pid.trim().parse().ok()?, signal.trim().parse().ok()?))
}

/// One host button of the process page; the active host is highlighted.
fn host_button(label: String, index: usize, active: usize) -> Element<'static, Message> {
    let class = if index == active {
        cosmic::theme::Button::Suggested
    } else {
        cosmic::theme::Button::Text
    };
    button::custom(text::caption(label).size(11.0))
        .padding([3u16, 8u16])
        .class(class)
        .on_press(Message::ProcessHost(index))
        .into()
}

/// One sort button; the active column is highlighted.
fn sort_button(key: SortKey, active: SortKey) -> Element<'static, Message> {
    let class = if key == active {
        cosmic::theme::Button::Suggested
    } else {
        cosmic::theme::Button::Text
    };
    button::custom(text::caption(key.label()).size(11.0))
        .padding([3u16, 8u16])
        .class(class)
        .on_press(Message::ProcessSort(key))
        .into()
}

/// One fixed-width cell of the process table; numbers are aligned right.
fn process_cell(
    value: String,
    width: f32,
    end: bool,
    heading: bool,
) -> Element<'static, Message> {
    let content: Element<'static, Message> = if heading {
        text::caption(value).into()
    } else {
        text::monotext(value).size(11.0).into()
    };
    container(content)
        .width(Length::Fixed(width))
        .align_x(if end { Alignment::End } else { Alignment::Start })
        .into()
}

/// Shortens a label so it fits its fixed-width column.
fn truncate_label(value: &str, max: usize) -> String {
    let text_value = value.to_owned();
    let length = text_value.chars().count();
    if length <= max {
        return text_value;
    }
    // Rows are right-aligned, so the end of a name carries the information that
    // tells two similar cards apart ("…Radeon 610M"): keep the end, cut the front.
    let skip = length - max + 1;
    let start = text_value
        .char_indices()
        .nth(skip)
        .map(|(index, _)| index)
        .unwrap_or(0);
    format!("…{}", &text_value[start..])
}

/// `--install-agent HOST` runs the installer without the UI and exits.
fn install_request_from_args() -> Option<InstallRequest> {
    let mut request = InstallRequest {
        port: 22,
        user: std::env::var("USER").unwrap_or_else(|_| "root".to_owned()),
        ..InstallRequest::default()
    };
    let mut args = std::env::args().skip(1);
    let mut found = false;
    while let Some(item) = args.next() {
        match item.as_str() {
            "--install-agent" => {
                request.host = args.next().unwrap_or_default();
                found = true;
            }
            "--install-user" => request.user = args.next().unwrap_or_default(),
            "--install-port" => {
                request.port = args.next().and_then(|value| value.parse().ok()).unwrap_or(22)
            }
            "--install-password-file" => {
                request.password = args
                    .next()
                    .and_then(|path| std::fs::read_to_string(path).ok())
                    .map(|value| value.trim_end().to_owned())
                    .unwrap_or_default();
            }
            _ => {}
        }
    }
    if found {
        Some(request)
    } else {
        None
    }
}

fn main() -> cosmic::iced::Result {
    if let Some(request) = install_request_from_args() {
        std::process::exit(install::run_cli(request));
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .try_init()
        .ok();

    cosmic::applet::run::<Sysmon>(())
}

/// A symbolic icon followed by a value, used for one panel entry.
fn metric(name: &'static str, value: String) -> Element<'static, Message> {
    Row::new()
        .align_y(Alignment::Center)
        .spacing(cosmic::theme::spacing().space_xxs)
        .push(icon_element(name, 16))
        .push(text::body(value))
        .into()
}

/// One label/value row: the label fills the room up to the column rule and ends
/// there, the value ends at the right edge of the window. Both right-aligned, so
/// every row of the popup is bounded by the same two edges.
fn value_row(
    icon: &'static str,
    label: &str,
    value: &str,
    value_characters: usize,
) -> Element<'static, Message> {
    // Same raster as the card table: icon | label (grows) | rule | value column.
    // The value column is exactly as wide as a real value needs — the width is
    // computed once, in `text_width`.
    table_row(vec![
        icon_element(icon, 16),
        grow_cell(label.to_owned(), Alignment::Start),
        fixed_cell(
            value.to_owned(),
            text_width(value_characters),
            Alignment::End,
        ),
    ])
}

/// Quiet heading of a block inside a section: small, uppercase, with a short
/// explanation on the right.
fn subsection_heading(title: &str) -> Element<'static, Message> {
    Row::new()
        .align_y(Alignment::Center)
        .width(Length::Fill)
        .push(
            text::caption(title.to_owned())
                .size(12.0)
                .class(cosmic::theme::Text::Color(HEADER_COLOR)),
        )
        .into()
}

// ---- table building -------------------------------------------------------

/// Column widths of the graphics card table, in pixels.
///
/// These are the *minimum* widths: every column grows proportionally when the
/// popup is wider, so the table always spans the whole window instead of
/// floating in the middle of it. The name column has the largest share, the
/// numeric columns keep their numbers right-aligned at their right edge.
/// Width of the numeric columns, derived from the longest text each column can
/// ever show — header included. Nothing is guessed: every value below is the
/// character count of a real string times the measured character width.
/// Leading column of a card row: colour mark (3 px) plus a small card icon
/// (14 px). Exactly as wide as the two together, so the icon keeps its size.
const GPU_COL_LEAD: f32 = MARK_WIDTH + 14.0 + 3.0;

const GPU_COL_LOAD: f32 = text_width(6);   // "100" bzw. Kopf "Last %"
const GPU_COL_TEMP: f32 = text_width(10);  // "100 °C" bzw. Kopf "Temp. °C"
const GPU_COL_POWER: f32 = text_width(10); // "100.0 W" bzw. Kopf "Watt"
const GPU_COL_VRAM: f32 = text_width(14);  // "100/100 GiB" bzw. Kopf "VRAM"
const GPU_COL_VRAM_PCT: f32 = text_width(6); // "100 %" bzw. Kopf "%"

/// Smallest window the popup may have: below this the card table would have to
/// squeeze, so the applet asks for at least this width.
const POPUP_MIN_WIDTH: f32 = 32.0
    + MARK_WIDTH
    + text_width(24) // narrowest useful name column
    + GPU_COL_LOAD
    + GPU_COL_TEMP
    + GPU_COL_POWER
    + GPU_COL_VRAM
    + GPU_COL_VRAM_PCT
    + 6.0 * TABLE_GAP as f32
    + 5.0 * (TABLE_GAP as f32 + 1.0);

/// Pixel width of one monospace character, measured from the running applet
/// (50 characters fitted into the former 420 px of text). Used for every column
/// width, so all widths come from one number instead of being guessed.
const CHAR_PX: f32 = 7.8;

/// Pixel width of `characters` monospace characters.
const fn text_width(characters: usize) -> f32 {
    characters as f32 * CHAR_PX
}

/// Header row of the graphics card table.
fn gpu_table_header() -> Element<'static, Message> {
    // Exactly the cells of a card row — leading column, name, five numbers — so
    // every header sits over its own column and the rules line up.
    table_row(vec![
        space::horizontal().width(Length::Fixed(GPU_COL_LEAD)).into(),
        head_grow("Karte", Alignment::Start),
        fixed_head("Last %", GPU_COL_LOAD),
        fixed_head("Temp. °C", GPU_COL_TEMP),
        fixed_head("Watt", GPU_COL_POWER),
        fixed_head("VRAM", GPU_COL_VRAM),
        fixed_head("%", GPU_COL_VRAM_PCT),
    ])
}

/// One graphics card as a table row.
///
/// `label` is what stands in the name column, so the same row builder serves the
/// local cards (numbered) and the cards of a monitored node.
fn gpu_table_row(
    label: &str,
    load: f32,
    temp_c: f32,
    watts: f32,
    vram_used: f32,
    vram_total: f32,
    color: Option<Color>,
) -> Element<'static, Message> {
    // Name column with a fixed width: the name starts at the left edge like every
    // other label, only the numbers follow the growing columns.
    let name: Element<'static, Message> = match color {
        Some(color) => container(
            text::monotext(truncate_label(label, 34))
                .size(13.0)
                .wrapping(Wrapping::None)
                .class(cosmic::theme::Text::Color(color)),
        )
        .width(Length::Fill)
        .align_x(Alignment::Start)
        .into(),
        None => grow_cell(truncate_label(label, 34), Alignment::Start),
    };
    // No zero padding: a value keeps its own digits and is only aligned to the
    // right edge of its column, so `4` and `100` stand under each other without
    // fake leading zeros.
    let temperature = if temp_c > 0.0 {
        format!("{temp_c:.0} °C")
    } else {
        "—".to_owned()
    };
    let power = if watts > 0.0 {
        format!("{watts:.1} W")
    } else {
        "—".to_owned()
    };
    let vram = if vram_total > 0.0 {
        format!("{vram_used:.0}/{vram_total:.0} GiB")
    } else if vram_used > 0.0 {
        format!("{vram_used:.0} GiB")
    } else {
        "—".to_owned()
    };
    let vram_pct = if vram_total > 0.0 {
        format!("{:.0}", percent_of(vram_used, vram_total))
    } else {
        "—".to_owned()
    };

    // Leading column: the colour that ties the row to its curve, then a small
    // card icon so every row starts with the same two marks as the other blocks.
    let lead: Element<'static, Message> = Row::new()
        .align_y(Alignment::Center)
        .spacing(0)
        .width(Length::Fixed(GPU_COL_LEAD))
        .push(match color {
            Some(color) => colored_mark(color),
            // Empty space where a card row shows its colour mark.
            None => space::horizontal().width(Length::Fixed(MARK_WIDTH)).into(),
        })
        .push(icon_element(ICON_GPU, 14))
        .into();

    table_row(vec![
        lead,
        name,
        fixed_cell(format!("{load:.0}"), GPU_COL_LOAD, Alignment::End),
        fixed_cell(temperature, GPU_COL_TEMP, Alignment::End),
        fixed_cell(power, GPU_COL_POWER, Alignment::End),
        fixed_cell(vram, GPU_COL_VRAM, Alignment::End),
        fixed_cell(vram_pct, GPU_COL_VRAM_PCT, Alignment::End),
    ])
}

/// Percentage of `used` in `total`, clamped to 0..100.
fn percent_of(used: f32, total: f32) -> f32 {
    if total > 0.0 {
        (used / total * 100.0).clamp(0.0, 100.0)
    } else {
        0.0
    }
}

/// One row of the card table, from any source (this machine or a node).
struct CardRow {
    name: String,
    load: f32,
    temp_c: f32,
    watts: f32,
    vram_used: f32,
    vram_total: f32,
}

/// The card table, used for this machine and for every monitored node, so both
/// look exactly the same.
fn card_table(rows: Vec<CardRow>) -> Element<'static, Message> {
    let multiple = rows.len() > 1;
    let mut column = Column::new()
        .spacing(TABLE_ROW_GAP)
        .width(Length::Fill)
        .push(gpu_table_header())
        .push(space::vertical().height(Length::Fixed(f32::from(TABLE_HEAD_GAP))));

    for (index, row) in rows.iter().enumerate() {
        column = column.push(gpu_table_row(
            &row.name,
            row.load,
            row.temp_c,
            row.watts,
            row.vram_used,
            row.vram_total,
            Some(COLOR_GPU_CHIPS[index % COLOR_GPU_CHIPS.len()]),
        ));
    }

    if multiple {
        let vram_used: f32 = rows.iter().map(|row| row.vram_used).sum();
        let vram_total: f32 = rows.iter().map(|row| row.vram_total).sum();
        let watts: f32 = rows.iter().map(|row| row.watts).sum();
        let load = rows.iter().map(|row| row.load).fold(0.0_f32, f32::max);
        let temp = rows.iter().map(|row| row.temp_c).fold(0.0_f32, f32::max);
        column = column
            .push(space::vertical().height(Length::Fixed(3.0)))
            .push(table_rule())
            .push(gpu_table_row(
                "Gesamt / Maximum",
                load,
                temp,
                watts,
                vram_used,
                vram_total,
                None,
            ));
    }

    column.into()
}

/// Table of the graphics cards of this machine.
fn local_gpu_table(sample: &sensors::Sample) -> Element<'static, Message> {
    let multiple = sample.gpus.len() > 1;
    let rows = sample
        .gpus
        .iter()
        .map(|gpu| CardRow {
            name: if multiple {
                gpu.short.clone()
            } else {
                gpu.name.clone()
            },
            load: gpu.load,
            temp_c: gpu.temp_c,
            watts: gpu.watts,
            vram_used: gpu.vram_used,
            vram_total: gpu.vram_total,
        })
        .collect();
    card_table(rows)
}

/// Table of the cards a monitored node reports.
fn remote_gpu_table(agent: &crate::spark::AgentState) -> Element<'static, Message> {
    if agent.node.gpus.is_empty() {
        // Agents before 2.3 send only the aggregated values.
        return card_table(vec![CardRow {
            name: if agent.gpu_name.is_empty() {
                "GPU".to_owned()
            } else {
                agent.gpu_name.clone()
            },
            load: agent.node.gpu_util_pct,
            temp_c: agent.node.gpu_temp_c,
            watts: agent.node.gpu_power_w,
            vram_used: (agent.gpu_vram_used_mib / 1024.0) as f32,
            vram_total: (agent.gpu_vram_total_mib / 1024.0) as f32,
        }]);
    }
    card_table(
        agent
            .node
            .gpus
            .iter()
            .map(|gpu| CardRow {
                name: gpu.name.clone(),
                load: gpu.util_pct,
                temp_c: gpu.temp_c,
                watts: gpu.power_w,
                vram_used: (gpu.vram_used_mib / 1024.0) as f32,
                vram_total: (gpu.vram_total_mib / 1024.0) as f32,
            })
            .collect(),
    )
}

/// A numeric cell of fixed width, aligned as asked, never wrapping.
fn fixed_cell(
    text_value: impl Into<String>,
    width: f32,
    alignment: Alignment,
) -> Element<'static, Message> {
    container(
        text::monotext(text_value.into())
            .size(13.0)
            .wrapping(Wrapping::None),
    )
    .width(Length::Fixed(width))
    .align_x(alignment)
    .into()
}

/// A cell that takes a share of the row, so the numeric columns always start at
/// the same x no matter how long the name is.
fn grow_cell(text_value: impl Into<String>, alignment: Alignment) -> Element<'static, Message> {
    container(
        text::monotext(text_value.into())
            .size(13.0)
            .wrapping(Wrapping::None),
    )
    .width(Length::Fill)
    .align_x(alignment)
    .into()
}

/// Right-aligned in a field of `width` characters, without zero padding.
///
/// The widgets align themselves; this is the pure function behind the tests.
#[cfg(test)]
fn fill_right(text_value: impl Into<String>, width: usize) -> String {
    let content = text_value.into();
    let length = content.chars().count();
    if length < width {
        return format!("{}{}", " ".repeat(width - length), content);
    }
    content
}

/// The quiet monospace text every header cell shares.
fn head_text(text_value: impl Into<String>) -> cosmic::widget::Text<'static, cosmic::Theme> {
    text::monotext(text_value.into())
        .size(12.0)
        .wrapping(Wrapping::None)
        .class(cosmic::theme::Text::Color(HEADER_COLOR))
}

/// Header cell of the name column: grows with the row like the names below it.
fn head_grow(text_value: impl Into<String>, alignment: Alignment) -> Element<'static, Message> {
    container(head_text(text_value))
        .width(Length::Fill)
        .align_x(alignment)
        .into()
}

/// Header cell of a numeric column: exactly as wide as the values below it, so
/// header and numbers can never drift apart.
fn fixed_head(text_value: impl Into<String>, width: f32) -> Element<'static, Message> {
    container(head_text(text_value))
        .width(Length::Fixed(width))
        .align_x(Alignment::End)
        .into()
}

/// Thin rule that separates a total row from the rows above it.
fn table_rule() -> Element<'static, Message> {
    container(space::horizontal())
        .width(Length::Fill)
        .height(Length::Fixed(TABLE_RULE_WIDTH))
        .class(cosmic::theme::Container::Custom(Box::new(|theme: &cosmic::Theme| {
            cosmic::iced::widget::container::Style {
                background: Some(cosmic::iced::Background::Color(
                    theme.cosmic().bg_divider().into(),
                )),
                ..Default::default()
            }
        })))
        .into()
}

/// Thin vertical rule between two table columns.
/// One pixel wide and never wider: a rule without a fixed width grew across the
/// whole free space and its line ended up over the label text.
fn column_rule() -> Element<'static, Message> {
    container(divider::vertical::default())
        .width(Length::Fixed(1.0))
        .height(Length::Fixed(15.0))
        .into()
}

/// Number of cells a card row is built from: leading column, name and the five
/// numeric columns. The header must use the same number, otherwise its rules and
/// titles no longer sit over the values (that defect happened once).
#[cfg(test)]
const TABLE_CELLS: usize = 7;

/// Cells the header and every card row must supply. Used by the tests to keep
/// both in step.
const CARD_ROW_CELLS: usize = 7;

/// One table row from already formatted cells, separated by column rules.
fn table_row(cells: Vec<Element<'static, Message>>) -> Element<'static, Message> {
    let mut row = Row::new()
        .align_y(Alignment::Center)
        .spacing(TABLE_GAP)
        .width(Length::Fill);
    debug_assert!(
        cells.len() == CARD_ROW_CELLS || cells.len() <= 3,
        "Tabellenzeile mit {} Zellen gebaut, erwartet werden {CARD_ROW_CELLS}",
        cells.len()
    );
    for (index, cell_element) in cells.into_iter().enumerate() {
        if index > 0 {
            row = row.push(column_rule());
        }
        row = row.push(cell_element);
    }
    row.into()
}

/// Small coloured mark in front of a graphics card row.
///
/// The cell is exactly as wide as an icon in the other rows, so every row of the
/// popup — values, card table, node details — starts its first column at the same
/// x. Inside it the mark sits at the left edge, well away from the next rule.
fn colored_mark(color: Color) -> Element<'static, Message> {
    container(space::horizontal().width(Length::Fixed(3.0)))
        .width(Length::Fixed(MARK_WIDTH))
        .align_x(Alignment::Start)
        .height(Length::Fixed(14.0))
        .class(cosmic::theme::Container::Custom(Box::new(move |_theme: &cosmic::Theme| {
            cosmic::iced::widget::container::Style {
                background: Some(cosmic::iced::Background::Color(color)),
                border: cosmic::iced::border::rounded(1),
                ..Default::default()
            }
        })))
        .into()
}



/// Width check for the graphics card table: the longest possible content has to
/// fit into the popup, otherwise the row wraps and the table breaks apart.
#[cfg(test)]
mod table_layout_tests {
    use super::*;

    /// Left and right padding of the popup container.
    const POPUP_PADDING: f32 = 32.0;

    /// Content width available in a window of `window` pixels.
    fn content(window: f32) -> f32 {
        window - POPUP_PADDING
    }

    /// Everything in a card row that is not the name column.
    fn fixed_part() -> f32 {
        MARK_WIDTH
            + GPU_COL_LOAD
            + GPU_COL_TEMP
            + GPU_COL_POWER
            + GPU_COL_VRAM
            + GPU_COL_VRAM_PCT
            + 6.0 * f32::from(TABLE_GAP)
            + 5.0 * (f32::from(TABLE_GAP) + 1.0)
    }

    /// Name column width in a window of `window` pixels.
    fn name_space(window: f32) -> f32 {
        content(window) - fixed_part()
    }

    #[test]
    fn headers_fit_into_their_columns() {
        // The header of a column must never be wider than the column, otherwise
        // the layout engine wraps it and the column drifts away from its values.
        let cases = [
            ("Last %", GPU_COL_LOAD),
            ("Temp. °C", GPU_COL_TEMP),
            ("Watt", GPU_COL_POWER),
            ("VRAM", GPU_COL_VRAM),
            ("%", GPU_COL_VRAM_PCT),
        ];
        for (header, width) in cases {
            assert!(
                text_width(header.chars().count()) <= width,
                "Kopf {header:?} braucht {} px, die Spalte hat {width} px",
                text_width(header.chars().count())
            );
        }
    }

    #[test]
    fn values_fit_into_their_columns() {
        // Longest realistic values of the running machines.
        let cases = [
            ("100", GPU_COL_LOAD),
            ("100 °C", GPU_COL_TEMP),
            ("100.0 W", GPU_COL_POWER),
            ("100/100 GiB", GPU_COL_VRAM),
            ("100 %", GPU_COL_VRAM_PCT),
            ("—", GPU_COL_LOAD),
        ];
        for (value, width) in cases {
            assert!(
                text_width(value.chars().count()) <= width,
                "Wert {value:?} braucht {} px, die Spalte hat {width} px",
                text_width(value.chars().count())
            );
        }
    }

    #[test]
    fn the_window_is_wide_enough_for_every_card_name() {
        // The window grows with the longest card name, so the name column is
        // never squeezed below what a real name needs.
        for name in [
            "Radeon AI PRO R9700",
            "Radeon 610M",
            "NVIDIA GB10",
            "NVIDIA RTX A3000 Laptop GPU",
        ] {
            let needed = name.chars().count();
            let window = popup_width_for(needed);
            assert!(
                name_space(window) >= text_width(needed),
                "bei {name:?} bleiben nur {} px für {} Zeichen",
                name_space(window),
                needed
            );
        }
    }

    #[test]
    fn the_window_stays_within_its_limits() {
        // Below the minimum name width the window keeps its minimum size.
        assert_eq!(popup_width_for(0), POPUP_MIN_WIDTH);
        assert_eq!(popup_width_for(20), popup_width_for(0));
        // A very long name cannot push the window past its maximum: the name
        // column itself stops growing at 44 characters.
        let capped = popup_width_for(200);
        assert!(
            capped <= POPUP_MAX_WIDTH,
            "Obergrenze nicht eingehalten: {capped}"
        );
        assert_eq!(capped, popup_width_for(44));
        assert!(POPUP_MIN_WIDTH <= popup_width_for(33));
        assert!(popup_width_for(33) <= POPUP_MAX_WIDTH);
    }

    /// The value columns of the label/value rows must stay narrow: the rows share
    /// the window with the label, so an over-wide value column pushes the row out
    /// of the window (exactly what happened when the width was multiplied twice).
    #[test]
    fn value_rows_keep_room_for_their_label() {
        // value_characters passed at the call sites, plus the real longest text.
        let rows = [
            (9usize, "134.7 W"),
            (17, "95/126 GiB  76 %"),
            (15, "0.064 W · load"),
            (14, "keine erkannt"),
            (6, "12"),
        ];
        for (characters, longest) in rows {
            let column = text_width(characters);
            assert!(
                text_width(longest.chars().count()) <= column,
                "{longest:?} braucht {} px, die Spalte hat {column} px",
                text_width(longest.chars().count())
            );
            // The label still gets the larger part of the window.
            let label_space = content(760.0) - 16.0 - column - 2.0 * f32::from(TABLE_GAP) - 1.0;
            assert!(
                label_space >= text_width(30),
                "für die Beschriftung bleiben nur {label_space} px"
            );
        }
    }

    #[test]
    fn values_are_right_aligned_without_fake_zeros() {
        // A short value is moved to the right edge, never padded with zeros.
        assert_eq!(fill_right("4", 5), "    4");
        assert_eq!(fill_right("100", 5), "  100");
        // Overlong content is left untouched instead of being cut off silently.
        assert_eq!(fill_right("123456", 3), "123456");
        // `—` for a missing value also lands on the right edge.
        assert_eq!(fill_right("—", 5), "    —");
    }

    #[test]
    fn real_card_values_keep_their_own_digits() {
        // The exact texts the table builds for the cards of this machine: no
        // filler zeros, and nothing wider than its column.
        let values = [
            ("4", GPU_COL_LOAD),
            ("0", GPU_COL_LOAD),
            ("100", GPU_COL_LOAD),
            ("31 °C", GPU_COL_TEMP),
            ("10.0 W", GPU_COL_POWER),
            ("0.1 W", GPU_COL_POWER),
            ("21/32 GiB", GPU_COL_VRAM),
            ("2/2 GiB", GPU_COL_VRAM),
            ("67", GPU_COL_VRAM_PCT),
            ("100", GPU_COL_VRAM_PCT),
        ];
        for (value, width) in values {
            // A single `0` is a genuine value; `00`/`04` would be filler.
            let filled = value.len() > 1 && value.starts_with('0') && !value.starts_with("0.");
            assert!(!filled, "{value} darf nicht mit einer Fuell-Null beginnen");
            assert!(
                text_width(value.chars().count()) <= width,
                "{value} ist breiter als seine Spalte ({width} px)"
            );
        }
    }

    /// Header and data rows must be built from the same number of cells. With a
    /// different count the columns drift: the titles no longer sit over their
    /// values and the rules do not line up.
    #[test]
    fn header_and_rows_have_the_same_cells() {
        assert_eq!(TABLE_CELLS, 7, "Leitspalte, Name, fünf Zahlenspalten");
        // The header is built with exactly this many cells (see gpu_table_header)
        // and so is every card row (see gpu_table_row).
        let header_cells = 1 + 1 + 5;
        assert_eq!(header_cells, TABLE_CELLS);
    }

    /// Builds the text of the card table with the same rules the widget uses
    /// (fixed numeric columns, right-aligned, name column filling the rest), so
    /// the layout can be read as text:
    /// `cargo test --release table_preview -- --nocapture`
    #[test]
    fn table_preview() {
        let window = popup_width_for(33);
        let name_chars = (name_space(window) / CHAR_PX).floor() as usize;
        let column = |value: &str, width: f32| fill_right(value, (width / CHAR_PX).floor() as usize);
        let row = |name: &str, load: &str, temp: &str, watt: &str, vram: &str, pct: &str| {
            format!(
                "{} │ {} │ {} │ {} │ {} │ {} │ {}",
                " ".repeat(GPU_COL_LEAD as usize + 2),
                fill_right(name, name_chars),
                column(load, GPU_COL_LOAD),
                column(temp, GPU_COL_TEMP),
                column(watt, GPU_COL_POWER),
                column(vram, GPU_COL_VRAM),
                column(pct, GPU_COL_VRAM_PCT),
            )
        };
        println!("Fenster: {window} px | Namensspalte: {name_chars} Zeichen");
        println!("{}", row("Karte", "Last %", "Temp. °C", "Watt", "VRAM", "%"));
        println!("{}", "─".repeat(name_chars + 46));
        println!("{}", row("Radeon AI PRO R9700", "2", "31 °C", "8.0 W", "21/32 GiB", "67"));
        println!("{}", row("Radeon AI PRO R9700", "93", "45 °C", "145.0 W", "30/32 GiB", "94"));
        println!("{}", row("Radeon 610M", "0", "39 °C", "0.1 W", "2/2 GiB", "78"));
        println!("{}", row("Gesamt / Maximum", "93", "45 °C", "153.1 W", "53/66 GiB", "80"));
    }
}

/// Label/value row whose value carries the colour of a curve.
fn detail_row_colored(
    name: &'static str,
    label: impl Into<String>,
    value: String,
    color: Option<Color>,
) -> Element<'static, Message> {
    let value_text = match color {
        Some(color) => text::monotext(value)
            .class(cosmic::theme::Text::Color(color))
            .width(Length::Fill),
        None => text::monotext(value).width(Length::Fill),
    };
    Row::new()
        .align_y(Alignment::Center)
        .spacing(cosmic::theme::spacing().space_xs)
        .width(Length::Fill)
        .push(icon_element(name, 16))
        .push(text::body(label.into()).width(Length::Fill))
        .push(value_text)
        .into()
}

/// Shortens a processor name so that it fits next to the dots.
fn compact_cpu_label(name: &str) -> String {
    let mut label = name
        .replace(" Processor", "")
        .replace(" CPU", "")
        .replace("-Core", "")
        .replace("(R)", "")
        .replace("(TM)", "")
        .trim()
        .to_owned();
    if let Some(rest) = label.strip_prefix("ARM ") {
        label = rest.to_owned();
    }
    // "AMD Ryzen 7 3700X 8" -> drop the trailing core count left over from "8-Core".
    if let Some((head, tail)) = label.rsplit_once(' ') {
        if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) {
            label = head.to_owned();
        }
    }
    // "Cortex-A725 + Cortex-X925" reads better as "Cortex-A725/X925".
    if let Some((first, second)) = label.split_once(" + ") {
        if let (Some(a), Some(b)) = (first.strip_prefix("Cortex-"), second.strip_prefix("Cortex-")) {
            label = format!("Cortex-{a}/{b}");
        }
    }
    label
}

/// Hostname of this machine, used for the LOKAL heading.
fn local_hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|value| value.trim().to_owned())
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "lokal".to_owned())
}

fn caption_colored(text_value: String, color: Color) -> Element<'static, Message> {
    text::caption(text_value).class(cosmic::theme::Text::Color(color)).into()
}

/// Heading row that separates LOKAL from REMOTE.
fn section_heading(title: &str, detail: Option<&str>) -> Element<'static, Message> {
    let spacing = cosmic::theme::spacing();
    let mut row = Row::new()
        .align_y(Alignment::Center)
        .spacing(spacing.space_xs)
        .width(Length::Fill)
        .push(text::heading(title.to_owned()));
    if let Some(detail) = detail {
        row = row.push(
            container(text::caption(detail.to_owned()))
                .width(Length::Fill)
                .align_x(Alignment::End),
        );
    }
    row.into()
}

/// REMOTE section: one collapsible block per configured node.
///
/// The collapsed row carries the key figures; a click expands the details
/// (engine, models, tokens, clients, node values) without leaving the popup.
fn remote_section(
    snapshot: &SparkSnapshot,
    use_discovered: bool,
    client_names: &[(String, String)],
    expanded: &[String],
) -> Element<'static, Message> {
    let spacing = cosmic::theme::spacing();
    let online = snapshot.online_count();
    let total = snapshot.agents.len();
    let outdated = snapshot.outdated_agents();

    let detail = if !snapshot.enabled {
        "deaktiviert".to_owned()
    } else if total == 0 {
        "keine Knoten konfiguriert".to_owned()
    } else {
        let mut parts = vec![format!("{online}/{total} Agenten")];
        if snapshot.gpu_node_count() > 0 {
            parts.push(format!("{:.0} W GPU", snapshot.total_gpu_watts()));
        }
        match snapshot.client_total() {
            0 => {}
            1 => parts.push("1 Client".to_owned()),
            count => parts.push(format!("{count} Clients")),
        }
        parts.join(" · ")
    };
    let mut column = Column::new()
        .spacing(spacing.space_xxs)
        .push(section_heading("REMOTE", Some(detail.as_str())));

    if !snapshot.enabled {
        column = column.push(text::caption(
            "Remote-Überwachung ist aus (SYSMON_SPARK_AGENTS=off).",
        ));
        return column.into();
    }
    if total == 0 {
        column = column.push(text::caption(
            "Über das ⋯-Menü einen Knoten eintragen oder installieren.",
        ));
        return column.into();
    }

    // Small hint only: the update itself lives in the settings, where the host
    // overview marks the affected nodes.
    if !outdated.is_empty() {
        column = column.push(
            button::custom(
                Row::new()
                    .align_y(Alignment::Center)
                    .spacing(spacing.space_xxs)
                    .push(icon_element(ICON_UPDATE, 14))
                    .push(text::caption(format!(
                        "{} Agent-Update(s) – in den Einstellungen",
                        outdated.len()
                    ))),
            )
            .padding([0u16, 2u16])
            .class(cosmic::theme::Button::Text)
            .on_press(Message::ToggleSettings),
        );
    }
    for warning in snapshot.warnings() {
        column = column.push(caption_colored(format!("⚠ {warning}"), COLOR_WARN));
    }

    for (index, agent) in snapshot.agents.iter().enumerate() {
        let is_open = expanded.iter().any(|item| item == &agent.label);
        if index > 0 {
            column = column.push(divider::horizontal::light());
        }
        column = column.push(remote_node(
            snapshot,
            agent,
            is_open,
            use_discovered,
            client_names,
        ));
    }
    column.into()
}

/// One remote node: a clickable summary row plus its details when expanded.
fn remote_node(
    snapshot: &SparkSnapshot,
    agent: &crate::spark::AgentState,
    open: bool,
    use_discovered: bool,
    client_names: &[(String, String)],
) -> Element<'static, Message> {
    let spacing = cosmic::theme::spacing();
    let name = agent.display_name();
    let hot = snapshot.node_is_hot(agent);
    // Agent status (version, updates) lives in the settings; the popup only gets
    // a small hint when an update is available.
    let version_state = crate::install::agent_version_state(&agent.version);
    let outdated = version_state == AgentVersionState::Outdated;

    // Summary: what the user needs at a glance.
    let mut summary = Vec::new();
    if !agent.online {
        summary.push(if agent.status.is_empty() {
            "offline".to_owned()
        } else {
            format!("offline · {}", agent.status)
        });
    } else {
        let engine_label = agent
            .engines
            .iter()
            .find(|engine| engine.has_metrics)
            .or_else(|| agent.engines.first())
            .map(|engine| engine.label())
            .unwrap_or_else(|| "Engine".to_owned());
        if agent.engine.available {
            summary.push(format!(
                "{engine_label} · {:.1} tok/s · KV {:.0} %",
                agent.engine.gen_tok_s, agent.engine.kv_cache_pct
            ));
        } else if let Some(engine) = agent.engines.first() {
            let models = match engine.models.len() {
                0 => "Modelle unbekannt".to_owned(),
                1 => engine.models[0].clone(),
                count => format!("{count} Modelle"),
            };
            summary.push(format!("{} · {models}", engine.label()));
        } else if agent.engine.reason == "no-endpoint" {
            summary.push("kein lokaler LLM-Endpunkt".to_owned());
        }
        if agent.node.gpu_available {
            summary.push(format!("{:.0} W", agent.node.gpu_power_w));
        }
        if hot {
            summary.push(format!("GPU {:.0} °C", agent.node.gpu_temp_c));
        }
    }
    if outdated {
        summary.push(format!("Agent {}", agent.version));
    }

    let color = if !agent.online {
        COLOR_BAD
    } else if hot || outdated {
        COLOR_WARN
    } else {
        COLOR_RAM
    };

    let header = Row::new()
        .align_y(Alignment::Center)
        .spacing(spacing.space_xs)
        .width(Length::Fill)
        .push(icon_element(
            if open { ICON_CHEVRON_OPEN } else { ICON_CHEVRON_CLOSED },
            16,
        ))
        .push(text::body(name.clone()).width(Length::Shrink))
        .push(
            container(caption_colored(summary.join(" · "), color))
                .width(Length::Fill)
                .align_x(Alignment::End),
        );
    let header_button = button::custom(header)
        .width(Length::Fill)
        .padding([2u16, 4u16])
        .class(cosmic::theme::Button::Text)
        .on_press(Message::ToggleRemote(agent.label.clone()));

    let mut column = Column::new()
        .spacing(spacing.space_xxs)
        .push(header_button);

    if !open {
        return column.into();
    }

    if !agent.online {
        column = column.push(text::caption(format!(
            "Keine Verbindung ({}). Letzter Wert vor {:.0} s.",
            agent.status,
            agent.age_s.min(9999.0)
        )));
        return column.into();
    }

    // ---- details: label left, value right, dots between ----
    if agent.engine.available {
        column = column.push(value_row(ICON_SPARK, "Modell", &agent.engine.model, 15));
        column = column.push(value_row(
            ICON_POWER,
            "Token-Rate",
            &format!(
                "{:.1}/{:.1} tok/s",
                agent.engine.gen_tok_s, agent.engine.prompt_tok_s
            ),
            15,
        ));
        column = column.push(value_row(
            ICON_CPU,
            "TTFT (Ø)",
            &format!("{:.0} ms", agent.engine.ttft_ms),
            15,
        ));
        column = column.push(value_row(
            ICON_VRAM,
            "KV-Cache",
            &format!("{:.1} %", agent.engine.kv_cache_pct),
            15,
        ));
        column = column.push(value_row(
            ICON_GPU,
            "Anfragen",
            &format!(
                "{} aktiv · {} wartend",
                agent.engine.running, agent.engine.waiting
            ),
            15,
        ));
        if let Some(accept) = agent.engine.spec_accept_pct {
            column = column.push(value_row(
                ICON_GPU,
                "Spekulation",
                &format!("{accept:.0} % akzeptiert"),
                15,
            ));
        }
        if agent.engine.preemptions_total > 0 {
            column = column.push(detail_row_colored(
                ICON_POWER,
                "Präemptionen",
                format!("{}", agent.engine.preemptions_total),
                Some(COLOR_WARN),
            ));
        }
        // History of the engine that serves the metrics.
        if snapshot.gen_tok_s.len() >= 2 {
            let decode = snapshot.gen_tok_s.clone();
            let prefill = snapshot.prompt_tok_s.clone();
            let peak = decode
                .iter()
                .chain(prefill.iter())
                .fold(5.0_f32, |acc, value| acc.max(*value));
            let peak = (peak / 5.0).ceil() * 5.0;
            column = column.push(chart_block(
                "Token-Rate · letzte Stunde",
                vec![
                    Series::new(COLOR_TOKENS, decode.clone()),
                    Series::new(COLOR_PREFILL, prefill.clone()),
                ],
                peak.max(5.0),
                vec![
                    (COLOR_TOKENS, "Decode".to_owned()),
                    (COLOR_PREFILL, "Prefill".to_owned()),
                ],
                vec![
                    stats_line("Decode", &decode, " tok/s"),
                    stats_line("Prefill", &prefill, " tok/s"),
                ],
            ));
        }
        if snapshot.kv_pct.len() >= 2 {
            let kv = snapshot.kv_pct.clone();
            let peak = kv.iter().fold(10.0_f32, |acc, value| acc.max(*value));
            let peak = (peak / 10.0).ceil() * 10.0;
            column = column.push(chart_block(
                "KV-Cache · letzte Stunde",
                vec![Series::new(COLOR_RAM, kv.clone())],
                peak.max(10.0),
                vec![(COLOR_RAM, "KV-Cache".to_owned())],
                vec![stats_line("KV", &kv, " %")],
            ));
        }
    } else if agent.engine.reason == "no-endpoint" {
        // Already summarised in the node header; no second line needed here.
    } else if let Some(error) = agent.engine.error.clone().filter(|value| !value.is_empty()) {
        column = column.push(caption_colored(error, COLOR_BAD));
    }

    // ---- endpoints: one clickable line per engine ----
    if use_discovered && !agent.engines.is_empty() {
        for engine in &agent.engines {
            let loaded = if engine.loaded.is_empty() {
                String::new()
            } else {
                format!(" · geladen: {}", engine.loaded.join(", "))
            };
            let row = Row::new()
                .align_y(Alignment::Center)
                .spacing(spacing.space_xs)
                .width(Length::Fill)
                .push(icon_element(ICON_SPARK, 16))
                .push(
                    container(text::monotext(format!("{}", engine.port)).size(13.0))
                        .width(Length::Fixed(text_width(6)))
                        .align_x(Alignment::End),
                )
                .push(
                    container(text::monotext(engine.kind.clone()).size(13.0))
                        .width(Length::Fixed(text_width(12)))
                        .align_x(Alignment::Start),
                )
                .push(
                    container(text::caption(format!(
                        "{} Modelle{}",
                        engine.model_count,
                        if engine.has_metrics { " · Metriken" } else { "" }
                    )))
                    .width(Length::Fill)
                    .align_x(Alignment::End),
                )
                .push(icon_element(ICON_CHEVRON_OPEN, 14));
            column = column.push(
                button::custom(row)
                    .width(Length::Fill)
                    .padding([1u16, 2u16])
                    .class(cosmic::theme::Button::Text)
                    .on_press(Message::OpenModels(agent.label.clone(), engine.port)),
            );
            if !loaded.is_empty() {
                column = column.push(text::caption(loaded.trim_start_matches(" · ").to_owned()));
            }
        }
    }

    // ---- this machine: same row shape as the local block ----
    let cpu = if agent.cpu_model.is_empty() {
        "CPU".to_owned()
    } else {
        compact_cpu_label(&agent.cpu_model)
    };
    column = column.push(value_row(
        ICON_CPU,
        &cpu,
        &format!("{:.0} %", agent.node.cpu_pct),
        9,
    ));
    column = column.push(value_row(
        ICON_RAM,
        "Arbeitsspeicher",
        &format!(
            "{:.0}/{:.0} GiB  {:.0} %",
            agent.node.mem_used_gib, agent.node.mem_total_gib, agent.node.mem_pct
        ),
        17,
    ));
    if agent.node.gpu_available {
        column = column.push(remote_gpu_table(agent));
        column = column.push(value_row(
            ICON_POWER,
            "Auslastung (1 min)",
            &format!("{:.2}", agent.node.load1),
            9,
        ));
    } else {
        column = column.push(value_row(ICON_GPU, "Grafikkarten", "keine erkannt", 14));
    }

    if agent.client_port > 0 || !agent.clients.is_empty() {
        let total: u32 = agent.clients.iter().map(|client| client.count).sum();
        column = column.push(value_row(ICON_GPU, "Clients an der Engine", &format!("{total}"), 6));
        for client in &agent.clients {
            let label = client_names
                .iter()
                .find(|(ip, _)| ip == &client.ip)
                .map(|(_, name)| name.clone())
                .unwrap_or_else(|| client.ip.clone());
            column = column.push(text::caption(format!(
                "↳ {label}{}",
                if client.count > 1 {
                    format!(" ×{}", client.count)
                } else {
                    String::new()
                }
            )));
        }
    }

    column.into()
}

fn icon_element(name: &'static str, size: u16) -> Element<'static, Message> {
    icon::from_name(name).size(size).icon().into()
}

/// Title, chart, legend and statistics for one metric family.
fn chart_block(
    title: &'static str,
    series: Vec<Series>,
    max: f32,
    legend: Vec<(Color, String)>,
    stats: Vec<String>,
) -> Element<'static, Message> {
    let chart: Element<'static, Message> = Canvas::new(Chart::new(series, max))
        .width(Length::Fill)
        .height(Length::Fixed(CHART_HEIGHT))
        .into();

    let mut legend_row = Row::new()
        .spacing(cosmic::theme::spacing().space_s)
        .width(Length::Fill);
    for (color, label) in legend {
        legend_row = legend_row.push(
            text::caption(format!("— {label}")).class(cosmic::theme::Text::Color(color)),
        );
    }

    let mut stats_column = Column::new().spacing(2);
    for line in stats {
        stats_column = stats_column.push(text::caption(line));
    }

    Column::new()
        .spacing(cosmic::theme::spacing().space_xxs)
        .push(text::heading(title))
        .push(chart)
        .push(legend_row)
        .push(stats_column)
        .into()
}

fn stats_line(label: &str, values: &[f32], unit: &str) -> String {
    let (min, average, max) = stats(values);
    format!("{label}: jetzt {:.1}{unit} · Ø {:.1}{unit} · min {:.1}{unit} · max {:.1}{unit}", values.last().copied().unwrap_or(0.0), average, min, max)
}
