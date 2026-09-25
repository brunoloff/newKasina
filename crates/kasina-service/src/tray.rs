//! Desktop system-tray host for the persistent measurement service.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use kasina_protocol::v1::{ConnectionState, DeviceKind, RecordingState, StatusSnapshot};
use kasina_service::{ServicePaths, ServiceState};
use kasina_thoughtstream::{SerialPortChoice, ThoughtStreamPortSelection, available_serial_ports};
use parking_lot::RwLock;
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tokio::runtime::Handle;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use super::{Args, port_settings, run_service_thread};

const REFRESH_INTERVAL: Duration = Duration::from_millis(500);
const MENU_RECORDING: &str = "recording-action";
const MENU_SERVER: &str = "server-action";
const MENU_LAUNCH_APP: &str = "launch-app";
const MENU_OPEN_RECORDINGS: &str = "open-recordings";
const MENU_PORT_AUTO: &str = "thoughtstream-auto";
const MENU_PORT_REFRESH: &str = "thoughtstream-refresh";
const MENU_PORT_PREFIX: &str = "thoughtstream-port:";
const MENU_QUIT: &str = "quit";
const ICON_SIZE: u32 = 64;

#[derive(Debug, Clone)]
struct ServiceControl {
    state: Arc<ServiceState>,
    runtime: Handle,
    recordings_dir: PathBuf,
}

#[derive(Debug, Default)]
struct BridgeState {
    control: Option<ServiceControl>,
    notice: String,
}

/// Thread-safe link between the native tray event loop and the Tokio service runtime.
#[derive(Debug, Clone, Default)]
pub(crate) struct ServiceBridge {
    inner: Arc<RwLock<BridgeState>>,
}

impl ServiceBridge {
    pub(crate) fn install(
        &self,
        state: Arc<ServiceState>,
        runtime: Handle,
        recordings_dir: PathBuf,
    ) {
        let mut inner = self.inner.write();
        inner.control = Some(ServiceControl {
            state,
            runtime,
            recordings_dir,
        });
        inner.notice = "Measurement server ready".to_owned();
    }

    pub(crate) fn clear(&self, state: &Arc<ServiceState>) {
        let mut inner = self.inner.write();
        if inner
            .control
            .as_ref()
            .is_some_and(|control| Arc::ptr_eq(&control.state, state))
        {
            inner.control = None;
        }
    }

    fn control(&self) -> Option<ServiceControl> {
        self.inner.read().control.clone()
    }

    fn snapshot(&self) -> Option<StatusSnapshot> {
        self.control()
            .map(|control| control.state.status_snapshot())
    }

    fn has_control(&self) -> bool {
        self.inner.read().control.is_some()
    }

    fn notice(&self) -> String {
        self.inner.read().notice.clone()
    }

    fn set_notice(&self, notice: impl Into<String>) {
        self.inner.write().notice = notice.into();
    }

    fn toggle_recording(&self) {
        let Some(control) = self.control() else {
            self.set_notice("The measurement server is not running");
            return;
        };
        let recording_active = control
            .state
            .status_snapshot()
            .recording
            .as_ref()
            .is_some_and(|recording| {
                RecordingState::try_from(recording.state) == Ok(RecordingState::Recording)
            });
        let bridge = self.clone();
        if recording_active {
            self.set_notice("Stopping and syncing recording…");
            control.runtime.spawn(async move {
                match control.state.stop_recording().await {
                    Ok(recording) => bridge.set_notice(format!(
                        "Recording completed · {} samples · {} dropped",
                        recording.sample_count, recording.dropped_samples
                    )),
                    Err(error) => bridge.set_notice(format!("Could not stop recording: {error}")),
                }
            });
        } else {
            self.set_notice("Starting recording…");
            control.runtime.spawn(async move {
                match control
                    .state
                    .start_recording(
                        "Tray recording".to_owned(),
                        "Started from the newKasina system tray".to_owned(),
                    )
                    .await
                {
                    Ok(_) => bridge.set_notice("Recording raw sensor data"),
                    Err(error) => bridge.set_notice(format!("Could not start recording: {error}")),
                }
            });
        }
    }

    fn recordings_dir(&self) -> Option<PathBuf> {
        self.control().map(|control| control.recordings_dir.clone())
    }
}

#[derive(Debug)]
struct RunningService {
    cancellation: CancellationToken,
    thread: JoinHandle<Result<()>>,
    stopping: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServiceLifecycle {
    Stopped,
    Starting,
    Running,
    Stopping,
    Error,
}

#[derive(Debug)]
struct ServiceRunner {
    args: Args,
    bridge: ServiceBridge,
    running: Option<RunningService>,
    failed: bool,
}

impl ServiceRunner {
    fn new(args: Args, bridge: ServiceBridge) -> Self {
        Self {
            args,
            bridge,
            running: None,
            failed: false,
        }
    }

    fn start(&mut self) {
        if self.running.is_some() {
            return;
        }
        self.failed = false;
        self.bridge.set_notice("Starting measurement server…");
        let cancellation = CancellationToken::new();
        let thread_cancellation = cancellation.clone();
        let args = self.args.clone();
        let bridge = self.bridge.clone();
        match thread::Builder::new()
            .name("kasina-service-runtime".to_owned())
            .spawn(move || run_service_thread(args, thread_cancellation, Some(bridge)))
        {
            Ok(thread) => {
                self.running = Some(RunningService {
                    cancellation,
                    thread,
                    stopping: false,
                });
            }
            Err(error) => {
                self.failed = true;
                self.bridge
                    .set_notice(format!("Could not start server thread: {error}"));
            }
        }
    }

    fn stop(&mut self) {
        if let Some(running) = &mut self.running
            && !running.stopping
        {
            running.stopping = true;
            self.bridge
                .set_notice("Stopping measurement server cleanly…");
            running.cancellation.cancel();
        }
    }

    fn poll(&mut self) {
        let finished = self
            .running
            .as_ref()
            .is_some_and(|running| running.thread.is_finished());
        if !finished {
            return;
        }
        let running = self.running.take().expect("checked running service");
        let requested_stop = running.stopping;
        match running.thread.join() {
            Ok(Ok(())) => {
                self.failed = false;
                if requested_stop {
                    self.bridge.set_notice("Measurement server stopped");
                } else {
                    self.bridge.set_notice("Measurement server exited");
                }
            }
            Ok(Err(error)) => {
                self.failed = true;
                self.bridge
                    .set_notice(format!("Measurement server failed: {error:#}"));
            }
            Err(_) => {
                self.failed = true;
                self.bridge.set_notice("Measurement server thread panicked");
            }
        }
    }

    fn lifecycle(&self) -> ServiceLifecycle {
        match &self.running {
            Some(running) if running.stopping => ServiceLifecycle::Stopping,
            Some(_) if self.bridge.has_control() => ServiceLifecycle::Running,
            Some(_) => ServiceLifecycle::Starting,
            None if self.failed => ServiceLifecycle::Error,
            None => ServiceLifecycle::Stopped,
        }
    }
}

impl Drop for ServiceRunner {
    fn drop(&mut self) {
        if let Some(running) = &self.running {
            running.cancellation.cancel();
        }
    }
}

#[derive(Debug, Clone)]
enum UserEvent {
    Menu(MenuEvent),
    SerialPorts(std::result::Result<Vec<SerialPortChoice>, String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrafficState {
    Off,
    Connecting,
    Connected,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VisualState {
    heart: TrafficState,
    breath: TrafficState,
    thought: TrafficState,
}

#[derive(Debug)]
struct DeviceSummary {
    light: TrafficState,
    text: String,
}

#[derive(Debug)]
struct RecordingSummary {
    active: bool,
    available: bool,
    text: String,
}

struct TrayUi {
    _menu: Menu,
    tray: TrayIcon,
    service_status: MenuItem,
    heart_status: MenuItem,
    breath_status: MenuItem,
    thoughtstream_status: MenuItem,
    port_menu: Submenu,
    port_choices: Vec<SerialPortChoice>,
    displayed_port: Option<String>,
    ports_loaded: bool,
    recording_status: MenuItem,
    notice: MenuItem,
    recording_action: MenuItem,
    server_action: MenuItem,
    launch_app: MenuItem,
    last_visual: VisualState,
    last_tooltip: String,
}

impl TrayUi {
    fn new() -> Result<Self> {
        let title = MenuItem::new("newKasina measurement service", false, None);
        let service_status = MenuItem::new("Service: Starting", false, None);
        let heart_status = MenuItem::new("♥ Polar H10: Starting", false, None);
        let breath_status = MenuItem::new("≋ Go Direct: Starting", false, None);
        let thoughtstream_status = MenuItem::new("ThoughtStream USB: Starting", false, None);
        let port_menu = Submenu::new("Choose ThoughtStream port…", true);
        port_menu.append(&MenuItem::new("Looking for serial ports…", false, None))?;
        let recording_status = MenuItem::new("Recording: Waiting for service", false, None);
        let notice = MenuItem::new("Starting measurement server…", false, None);
        let recording_action = MenuItem::with_id(MENU_RECORDING, "Start recording", false, None);
        let server_action = MenuItem::with_id(MENU_SERVER, "Stop measurement server", false, None);
        let launch_app = MenuItem::with_id(MENU_LAUNCH_APP, "Open newKasina app", false, None);
        let open_recordings =
            MenuItem::with_id(MENU_OPEN_RECORDINGS, "Open recordings folder", true, None);
        let quit = MenuItem::with_id(MENU_QUIT, "Quit tray and server", true, None);
        let first_separator = PredefinedMenuItem::separator();
        let second_separator = PredefinedMenuItem::separator();
        let menu = Menu::with_items(&[
            &title,
            &service_status,
            &heart_status,
            &breath_status,
            &thoughtstream_status,
            &port_menu,
            &recording_status,
            &notice,
            &first_separator,
            &recording_action,
            &launch_app,
            &open_recordings,
            &server_action,
            &second_separator,
            &quit,
        ])
        .context("build tray menu")?;
        let last_visual = VisualState {
            heart: TrafficState::Connecting,
            breath: TrafficState::Connecting,
            thought: TrafficState::Connecting,
        };
        let last_tooltip = "newKasina · measurement server starting".to_owned();
        let tray = TrayIconBuilder::new()
            .with_id("newkasina-service")
            .with_menu(Box::new(menu.clone()))
            .with_menu_on_left_click(false)
            .with_menu_on_right_click(true)
            .with_tooltip(&last_tooltip)
            .with_icon(tray_icon(last_visual)?)
            .build()
            .context("create system tray icon")?;
        Ok(Self {
            _menu: menu,
            tray,
            service_status,
            heart_status,
            breath_status,
            thoughtstream_status,
            port_menu,
            port_choices: Vec::new(),
            displayed_port: None,
            ports_loaded: false,
            recording_status,
            notice,
            recording_action,
            server_action,
            launch_app,
            last_visual,
            last_tooltip,
        })
    }

    fn refresh_ports(
        &mut self,
        choices: Vec<SerialPortChoice>,
        selected: Option<String>,
    ) -> Result<()> {
        if self.ports_loaded && self.port_choices == choices && self.displayed_port == selected {
            return Ok(());
        }
        while self.port_menu.remove_at(0).is_some() {}
        self.port_menu.append(&CheckMenuItem::with_id(
            MENU_PORT_AUTO,
            "Find automatically",
            true,
            selected.is_none(),
            None,
        ))?;
        self.port_menu.append(&MenuItem::with_id(
            MENU_PORT_REFRESH,
            "Refresh port list",
            true,
            None,
        ))?;
        self.port_menu.append(&PredefinedMenuItem::separator())?;
        if choices.is_empty() {
            self.port_menu.append(&MenuItem::new(
                "No serial ports found — plug in the USB device",
                false,
                None,
            ))?;
        }
        let other_ports = Submenu::new("Other serial ports", true);
        for choice in &choices {
            let item = CheckMenuItem::with_id(
                format!("{MENU_PORT_PREFIX}{}", choice.path),
                &choice.label,
                true,
                selected.as_ref() == Some(&choice.path),
                None,
            );
            if choice.usb {
                self.port_menu.append(&item)?;
            } else {
                other_ports.append(&item)?;
            }
        }
        if !other_ports.items().is_empty() {
            self.port_menu.append(&other_ports)?;
        }
        if let Some(path) = &selected
            && !choices.iter().any(|choice| &choice.path == path)
        {
            self.port_menu.append(&CheckMenuItem::with_id(
                format!("{MENU_PORT_PREFIX}{path}"),
                format!("Saved port (unavailable): {path}"),
                true,
                true,
                None,
            ))?;
        }
        self.port_choices = choices;
        self.displayed_port = selected;
        self.ports_loaded = true;
        Ok(())
    }

    fn refresh(
        &mut self,
        lifecycle: ServiceLifecycle,
        status: Option<&StatusSnapshot>,
        notice: &str,
    ) {
        let heart = device_summary(status, DeviceKind::Polar, lifecycle);
        let breath = device_summary(status, DeviceKind::GoDirect, lifecycle);
        let thoughtstream = device_summary(status, DeviceKind::ThoughtStream, lifecycle);
        let recording = recording_summary(status);
        let service = service_summary(lifecycle, status);
        set_menu_text(&self.service_status, &format!("Service: {service}"));
        set_menu_text(&self.heart_status, &format!("♥ Polar H10: {}", heart.text));
        set_menu_text(
            &self.breath_status,
            &format!("≋ Go Direct: {}", breath.text),
        );
        set_menu_text(
            &self.thoughtstream_status,
            &format!("ThoughtStream USB: {}", thoughtstream.text),
        );
        set_menu_text(
            &self.recording_status,
            &format!("Recording: {}", recording.text),
        );
        set_menu_text(
            &self.notice,
            &if notice.is_empty() {
                "Ready".to_owned()
            } else {
                shorten(notice, 110)
            },
        );

        set_menu_text(
            &self.recording_action,
            if recording.active {
                "Stop and save recording"
            } else {
                "Start recording"
            },
        );
        self.recording_action
            .set_enabled(lifecycle == ServiceLifecycle::Running && recording.available);
        match lifecycle {
            ServiceLifecycle::Stopped | ServiceLifecycle::Error => {
                set_menu_text(&self.server_action, "Start measurement server");
                self.server_action.set_enabled(true);
            }
            ServiceLifecycle::Running => {
                set_menu_text(&self.server_action, "Stop measurement server");
                self.server_action.set_enabled(true);
            }
            ServiceLifecycle::Starting => {
                set_menu_text(&self.server_action, "Measurement server starting…");
                self.server_action.set_enabled(false);
            }
            ServiceLifecycle::Stopping => {
                set_menu_text(&self.server_action, "Measurement server stopping…");
                self.server_action.set_enabled(false);
            }
        }
        self.launch_app
            .set_enabled(lifecycle == ServiceLifecycle::Running);

        let visual = VisualState {
            heart: heart.light,
            breath: breath.light,
            thought: thoughtstream.light,
        };
        if visual != self.last_visual {
            match tray_icon(visual)
                .and_then(|icon| self.tray.set_icon(Some(icon)).map_err(anyhow::Error::from))
            {
                Ok(()) => self.last_visual = visual,
                Err(error) => warn!(%error, "could not update tray icon"),
            }
        }
        let tooltip = format!(
            "newKasina · Heart: {} · Breath: {} · ThoughtStream: {} · {}",
            short_state(heart.light),
            short_state(breath.light),
            short_state(thoughtstream.light),
            if recording.active {
                "recording"
            } else {
                "not recording"
            }
        );
        if tooltip != self.last_tooltip {
            if let Err(error) = self.tray.set_tooltip(Some(&tooltip)) {
                warn!(%error, "could not update tray tooltip");
            } else {
                self.last_tooltip = tooltip;
            }
        }
    }
}

/// Run the default desktop tray host. This function owns the native event loop.
pub(crate) fn run(args: Args) -> Result<()> {
    let defaults = ServicePaths::for_user()?;
    let fallback_recordings = args.recordings_dir.clone().unwrap_or(defaults.recordings);
    let selection = args
        .thoughtstream_selection
        .clone()
        .unwrap_or_else(|| ThoughtStreamPortSelection::new(args.thoughtstream_port.clone()));
    let settings_path = port_settings::settings_path()?;
    let bridge = ServiceBridge::default();
    let mut runner = ServiceRunner::new(args, bridge.clone());
    runner.start();

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let menu_proxy = proxy.clone();
    MenuEvent::set_event_handler(Some(move |event| {
        let _ignored = menu_proxy.send_event(UserEvent::Menu(event));
    }));
    let mut ui: Option<TrayUi> = None;
    let mut quit_when_stopped = false;
    let mut scanning_ports = false;
    let mut last_port_scan = Instant::now() - Duration::from_secs(5);

    event_loop.run(move |event, _target, control_flow| {
        *control_flow = ControlFlow::WaitUntil(Instant::now() + REFRESH_INTERVAL);
        match event {
            Event::NewEvents(StartCause::Init) => match TrayUi::new() {
                Ok(created) => {
                    ui = Some(created);
                    info!("newKasina system tray ready");
                }
                Err(error) => {
                    error!(%error, "could not create system tray");
                    runner.stop();
                    quit_when_stopped = true;
                }
            },
            Event::UserEvent(UserEvent::Menu(event)) => match event.id.as_ref() {
                MENU_PORT_AUTO => apply_port_choice(&selection, &settings_path, None, &bridge),
                MENU_PORT_REFRESH => {
                    last_port_scan = Instant::now() - Duration::from_secs(5);
                }
                id if id.starts_with(MENU_PORT_PREFIX) => {
                    if let Some(path) = id.strip_prefix(MENU_PORT_PREFIX) {
                        apply_port_choice(
                            &selection,
                            &settings_path,
                            Some(path.to_owned()),
                            &bridge,
                        );
                    }
                }
                MENU_RECORDING => bridge.toggle_recording(),
                MENU_SERVER => match runner.lifecycle() {
                    ServiceLifecycle::Stopped | ServiceLifecycle::Error => runner.start(),
                    ServiceLifecycle::Running => runner.stop(),
                    ServiceLifecycle::Starting | ServiceLifecycle::Stopping => {}
                },
                MENU_LAUNCH_APP => match launch_desktop_app() {
                    Ok(()) => bridge.set_notice("Opened newKasina app"),
                    Err(error) => bridge.set_notice(format!("Could not open app: {error:#}")),
                },
                MENU_OPEN_RECORDINGS => {
                    let recordings = bridge
                        .recordings_dir()
                        .unwrap_or_else(|| fallback_recordings.clone());
                    match open_directory(&recordings) {
                        Ok(()) => bridge.set_notice("Opened recordings folder"),
                        Err(error) => {
                            bridge.set_notice(format!("Could not open recordings: {error:#}"))
                        }
                    }
                }
                MENU_QUIT => {
                    quit_when_stopped = true;
                    runner.stop();
                }
                _ => {}
            },
            Event::UserEvent(UserEvent::SerialPorts(result)) => {
                scanning_ports = false;
                last_port_scan = Instant::now();
                match result {
                    Ok(ports) => {
                        if let Some(ui) = &mut ui
                            && let Err(error) = ui.refresh_ports(ports, selection.selected_port())
                        {
                            bridge.set_notice(format!("Could not update serial ports: {error:#}"));
                        }
                    }
                    Err(error) => {
                        if let Some(ui) = &mut ui {
                            // Keep the chooser and retry action usable even when enumeration fails.
                            let _ = ui
                                .refresh_ports(ui.port_choices.clone(), selection.selected_port());
                        }
                        bridge.set_notice(format!("Could not list serial ports: {error}"));
                    }
                }
            }
            Event::MainEventsCleared => {
                runner.poll();
                if !scanning_ports && last_port_scan.elapsed() >= Duration::from_secs(3) {
                    match scan_ports(proxy.clone()) {
                        Ok(()) => scanning_ports = true,
                        Err(error) => {
                            last_port_scan = Instant::now();
                            bridge.set_notice(format!("Could not scan serial ports: {error:#}"));
                        }
                    }
                }
                if let Some(ui) = &mut ui
                    && ui.ports_loaded
                    && let Err(error) =
                        ui.refresh_ports(ui.port_choices.clone(), selection.selected_port())
                {
                    bridge.set_notice(format!("Could not update port selection: {error:#}"));
                }
                if let Some(ui) = &mut ui {
                    let status = bridge.snapshot();
                    ui.refresh(runner.lifecycle(), status.as_ref(), &bridge.notice());
                }
                if quit_when_stopped
                    && matches!(
                        runner.lifecycle(),
                        ServiceLifecycle::Stopped | ServiceLifecycle::Error
                    )
                {
                    *control_flow = ControlFlow::Exit;
                }
            }
            Event::LoopDestroyed => runner.stop(),
            _ => {}
        }
    })
}

fn scan_ports(proxy: EventLoopProxy<UserEvent>) -> Result<()> {
    thread::Builder::new()
        .name("kasina-serial-ports".to_owned())
        .spawn(move || {
            let result = available_serial_ports().map_err(|error| format!("{error:#}"));
            let _ = proxy.send_event(UserEvent::SerialPorts(result));
        })?;
    Ok(())
}

fn apply_port_choice(
    selection: &ThoughtStreamPortSelection,
    settings_path: &Path,
    port: Option<String>,
    bridge: &ServiceBridge,
) {
    let saved = port_settings::save(settings_path, port.clone());
    selection.select(port);
    match saved {
        Ok(()) => {
            bridge.set_notice("ThoughtStream port selection saved; reconnecting ThoughtStream")
        }
        Err(error) => bridge.set_notice(format!(
            "Port changed for this run, but could not save the choice: {error:#}"
        )),
    }
}

fn device_summary(
    status: Option<&StatusSnapshot>,
    kind: DeviceKind,
    lifecycle: ServiceLifecycle,
) -> DeviceSummary {
    let Some(status) = status else {
        let (light, text) = match lifecycle {
            ServiceLifecycle::Starting | ServiceLifecycle::Stopping => {
                (TrafficState::Connecting, "Waiting for server")
            }
            ServiceLifecycle::Error => (TrafficState::Error, "Server error"),
            ServiceLifecycle::Stopped | ServiceLifecycle::Running => {
                (TrafficState::Off, "Server stopped")
            }
        };
        return DeviceSummary {
            light,
            text: text.to_owned(),
        };
    };
    let found = status.devices.iter().find(|device_status| {
        device_status.device.as_ref().is_some_and(|device| {
            DeviceKind::try_from(device.kind).is_ok_and(|device_kind| device_kind == kind)
        })
    });
    let Some(found) = found else {
        return DeviceSummary {
            light: TrafficState::Off,
            text: "Not configured".to_owned(),
        };
    };
    let connection = ConnectionState::try_from(found.state).unwrap_or_default();
    let (light, state) = match connection {
        ConnectionState::Connected => (TrafficState::Connected, "Connected"),
        ConnectionState::Discovering | ConnectionState::Connecting => {
            (TrafficState::Connecting, "Connecting")
        }
        ConnectionState::Reconnecting => (TrafficState::Connecting, "Reconnecting"),
        ConnectionState::Error | ConnectionState::Disconnected => {
            (TrafficState::Error, "Disconnected")
        }
        ConnectionState::Unspecified => (TrafficState::Off, "Unknown"),
    };
    let detail = shorten(&found.detail, 72);
    let mut text = if detail.is_empty() {
        state.to_owned()
    } else {
        format!("{state} · {detail}")
    };
    if connection == ConnectionState::Connected {
        text.push_str(&format!(
            " · {} ms sample age",
            found.last_sample_age_millis
        ));
    } else if found.reconnect_attempts > 0 {
        text.push_str(&format!(" · {} retries", found.reconnect_attempts));
    }
    DeviceSummary { light, text }
}

fn recording_summary(status: Option<&StatusSnapshot>) -> RecordingSummary {
    let Some(recording) = status.and_then(|status| status.recording.as_ref()) else {
        return RecordingSummary {
            active: false,
            available: false,
            text: "Waiting for server".to_owned(),
        };
    };
    let state = RecordingState::try_from(recording.state).unwrap_or_default();
    let active = state == RecordingState::Recording;
    let available = !matches!(
        state,
        RecordingState::Unspecified | RecordingState::Unavailable
    );
    let text = match state {
        RecordingState::Unspecified => "Unknown".to_owned(),
        RecordingState::Unavailable => "Unavailable".to_owned(),
        RecordingState::Idle => "Ready".to_owned(),
        RecordingState::Recording => format!(
            "Active · {} · {} samples · {} dropped",
            shorten(&recording.label, 40),
            recording.sample_count,
            recording.dropped_samples
        ),
        RecordingState::Completed => format!(
            "Last completed · {} samples · {} dropped",
            recording.sample_count, recording.dropped_samples
        ),
        RecordingState::Interrupted => format!(
            "Last interrupted · {} recovered samples",
            recording.sample_count
        ),
        RecordingState::Error => format!("Error · {}", shorten(&recording.detail, 72)),
    };
    RecordingSummary {
        active,
        available,
        text,
    }
}

fn service_summary(lifecycle: ServiceLifecycle, status: Option<&StatusSnapshot>) -> String {
    match (lifecycle, status) {
        (ServiceLifecycle::Running, Some(status)) => format!(
            "Running · {} uptime · {} clients",
            duration_label(status.uptime_millis),
            status.connected_clients
        ),
        (ServiceLifecycle::Starting, _) => "Starting".to_owned(),
        (ServiceLifecycle::Stopping, _) => "Stopping cleanly".to_owned(),
        (ServiceLifecycle::Error, _) => "Error".to_owned(),
        (ServiceLifecycle::Stopped, _) | (ServiceLifecycle::Running, None) => "Stopped".to_owned(),
    }
}

fn duration_label(milliseconds: u64) -> String {
    let seconds = milliseconds / 1_000;
    let hours = seconds / 3_600;
    let minutes = seconds % 3_600 / 60;
    let seconds = seconds % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}

fn set_menu_text(item: &MenuItem, text: &str) {
    let text = text.replace('&', "&&");
    if item.text() != text {
        item.set_text(text);
    }
}

fn shorten(text: &str, maximum: usize) -> String {
    let mut shortened = text.trim().chars().take(maximum).collect::<String>();
    if text.trim().chars().count() > maximum {
        shortened.push('…');
    }
    shortened
}

fn short_state(state: TrafficState) -> &'static str {
    match state {
        TrafficState::Off => "off",
        TrafficState::Connecting => "connecting",
        TrafficState::Connected => "connected",
        TrafficState::Error => "disconnected",
    }
}

fn launch_desktop_app() -> Result<()> {
    let executable_name = if cfg!(windows) {
        "kasina-app.exe"
    } else {
        "kasina-app"
    };
    let current = std::env::current_exe().context("locate tray executable")?;
    let sibling = current
        .parent()
        .context("tray executable has no parent directory")?
        .join(executable_name);
    let executable = if sibling.is_file() {
        sibling
    } else {
        PathBuf::from(executable_name)
    };
    Command::new(&executable)
        .spawn()
        .with_context(|| format!("launch {}", executable.display()))?;
    Ok(())
}

fn open_directory(path: &Path) -> Result<()> {
    if !path.is_dir() {
        return Err(anyhow!(
            "{} does not exist yet; start the server first",
            path.display()
        ));
    }
    #[cfg(target_os = "linux")]
    let mut command = Command::new("xdg-open");
    #[cfg(target_os = "windows")]
    let mut command = Command::new("explorer.exe");
    #[cfg(target_os = "macos")]
    let mut command = Command::new("open");
    command
        .arg(path)
        .spawn()
        .with_context(|| format!("open {}", path.display()))?;
    Ok(())
}

fn tray_icon(state: VisualState) -> Result<Icon> {
    Icon::from_rgba(icon_rgba(state), ICON_SIZE, ICON_SIZE)
        .map_err(|error| anyhow!("construct tray icon: {error}"))
}

// Keep the same three-glyph arrangement in the packaged service SVG.
const HEART_COLOR: [u8; 4] = [255, 75, 115, 255];
const BREATH_COLOR: [u8; 4] = [34, 211, 238, 255];
const THOUGHT_COLOR: [u8; 4] = [185, 105, 255, 255];
const INACTIVE_COLOR: [u8; 4] = [120, 120, 120, 255];
const ICON_BACKGROUND: [u8; 4] = [22, 22, 22, 255];

fn glyph_color(state: TrafficState, vivid: [u8; 4]) -> [u8; 4] {
    if state == TrafficState::Connected {
        vivid
    } else {
        INACTIVE_COLOR
    }
}

fn icon_rgba(state: VisualState) -> Vec<u8> {
    let mut pixels = vec![0_u8; (ICON_SIZE * ICON_SIZE * 4) as usize];
    let heart = glyph_color(state.heart, HEART_COLOR);
    let breath = glyph_color(state.breath, BREATH_COLOR);
    let thought = glyph_color(state.thought, THOUGHT_COLOR);
    // Supersampling keeps small tray silhouettes smooth without external assets.
    const SAMPLES: u32 = 4;
    for y in 0..ICON_SIZE {
        for x in 0..ICON_SIZE {
            let mut rgba = [0_u32; 4];
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let px = x as f32 + (sx as f32 + 0.5) / SAMPLES as f32;
                    let py = y as f32 + (sy as f32 + 0.5) / SAMPLES as f32;
                    let color = if heart_contains(px, py) {
                        heart
                    } else if breath_contains(px, py) {
                        breath
                    } else if thought_contains(px, py) {
                        thought
                    } else if circle_contains(
                        px,
                        py,
                        px.clamp(12.0, 52.0),
                        py.clamp(12.0, 52.0),
                        10.0,
                    ) {
                        ICON_BACKGROUND
                    } else {
                        [0; 4]
                    };
                    for channel in 0..3 {
                        rgba[channel] += u32::from(color[channel]) * u32::from(color[3]);
                    }
                    rgba[3] += u32::from(color[3]);
                }
            }
            let index = ((y * ICON_SIZE + x) * 4) as usize;
            for channel in 0..3 {
                pixels[index + channel] = rgba[channel].checked_div(rgba[3]).unwrap_or(0) as u8;
            }
            pixels[index + 3] = (rgba[3] / (SAMPLES * SAMPLES)) as u8;
        }
    }
    pixels
}

fn heart_contains(x: f32, y: f32) -> bool {
    let x = (x - 18.0) / 9.5;
    let y = (16.0 - y) / 8.5;
    (x * x + y * y - 1.0).powi(3) - x * x * y.powi(3) <= 0.0
}

fn breath_contains(x: f32, y: f32) -> bool {
    let wave_x = x.clamp(36.0, 55.0);
    let offset = ((wave_x - 36.0) * std::f32::consts::TAU / 20.0).sin() * 1.7;
    [11.0, 17.0, 23.0]
        .iter()
        .any(|base| circle_contains(x, y, wave_x, base + offset, 1.5))
}

fn thought_contains(x: f32, y: f32) -> bool {
    [
        (26.0, 43.0, 7.0),
        (33.0, 39.0, 8.0),
        (41.0, 43.0, 7.0),
        (39.0, 47.0, 6.0),
        (29.0, 48.0, 6.0),
        (17.0, 54.0, 2.3),
        (12.0, 58.0, 1.4),
    ]
    .iter()
    .any(|&(cx, cy, radius)| circle_contains(x, y, cx, cy, radius))
}

fn circle_contains(x: f32, y: f32, cx: f32, cy: f32, radius: f32) -> bool {
    (x - cx).powi(2) + (y - cy).powi(2) <= radius * radius
}

#[cfg(test)]
mod tests {
    use kasina_protocol::v1::{DeviceInfo, DeviceStatus, RecordingStatus};

    use super::*;

    #[test]
    fn each_connected_sensor_colors_only_its_own_glyph() {
        let off = VisualState {
            heart: TrafficState::Off,
            breath: TrafficState::Off,
            thought: TrafficState::Off,
        };
        let gray = icon_rgba(off);
        assert_eq!(gray.len(), (ICON_SIZE * ICON_SIZE * 4) as usize);
        assert_eq!(&gray[0..4], &[0, 0, 0, 0]);
        assert!(gray.chunks_exact(4).all(|p| p[0] == p[1] && p[1] == p[2]));
        for (state, active_point, color, inactive_points) in [
            (
                VisualState {
                    heart: TrafficState::Connected,
                    ..off
                },
                (18, 17),
                HEART_COLOR,
                [(43, 12), (33, 43)],
            ),
            (
                VisualState {
                    breath: TrafficState::Connected,
                    ..off
                },
                (43, 12),
                BREATH_COLOR,
                [(18, 17), (33, 43)],
            ),
            (
                VisualState {
                    thought: TrafficState::Connected,
                    ..off
                },
                (33, 43),
                THOUGHT_COLOR,
                [(18, 17), (43, 12)],
            ),
        ] {
            let icon = icon_rgba(state);
            assert_eq!(pixel(&icon, active_point.0, active_point.1), color);
            for (x, y) in inactive_points {
                assert_eq!(pixel(&icon, x, y), INACTIVE_COLOR);
            }
        }
        assert_eq!(
            icon_rgba(VisualState {
                heart: TrafficState::Connecting,
                breath: TrafficState::Error,
                thought: TrafficState::Off,
            }),
            gray
        );
    }

    #[test]
    fn device_and_recording_summaries_are_informative() {
        let status = StatusSnapshot {
            devices: vec![DeviceStatus {
                device: Some(DeviceInfo {
                    id: "polar:test".to_owned(),
                    name: "H10".to_owned(),
                    kind: DeviceKind::Polar as i32,
                    preferred: true,
                }),
                state: ConnectionState::Connected as i32,
                detail: "streaming".to_owned(),
                reconnect_attempts: 0,
                last_sample_age_millis: 42,
            }],
            recording: Some(RecordingStatus {
                state: RecordingState::Recording as i32,
                label: "Test".to_owned(),
                sample_count: 123,
                ..RecordingStatus::default()
            }),
            ..StatusSnapshot::default()
        };
        let heart = device_summary(Some(&status), DeviceKind::Polar, ServiceLifecycle::Running);
        let breath = device_summary(
            Some(&status),
            DeviceKind::GoDirect,
            ServiceLifecycle::Running,
        );
        let recording = recording_summary(Some(&status));
        assert_eq!(heart.light, TrafficState::Connected);
        assert!(heart.text.contains("42 ms sample age"));
        assert_eq!(breath.light, TrafficState::Off);
        assert!(recording.active);
        assert!(recording.text.contains("123 samples"));
    }

    #[test]
    fn labels_are_bounded_and_durations_are_stable() {
        assert_eq!(shorten("abcdef", 4), "abcd…");
        assert_eq!(duration_label(3_661_000), "01:01:01");
    }

    fn pixel(icon: &[u8], x: u32, y: u32) -> [u8; 4] {
        let index = ((y * ICON_SIZE + x) * 4) as usize;
        icon[index..index + 4].try_into().unwrap()
    }
}
