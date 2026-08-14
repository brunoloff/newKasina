//! Desktop system-tray host for the persistent measurement service.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use kasina_protocol::v1::{ConnectionState, DeviceKind, RecordingState, StatusSnapshot};
use kasina_service::{ServicePaths, ServiceState};
use parking_lot::RwLock;
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tokio::runtime::Handle;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use super::{Args, run_service_thread};

const REFRESH_INTERVAL: Duration = Duration::from_millis(500);
const MENU_RECORDING: &str = "recording-action";
const MENU_SERVER: &str = "server-action";
const MENU_LAUNCH_APP: &str = "launch-app";
const MENU_OPEN_RECORDINGS: &str = "open-recordings";
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
                    Ok(_) => bridge.set_notice("Recording raw breath and heartbeat data"),
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
            recording_status,
            notice,
            recording_action,
            server_action,
            launch_app,
            last_visual,
            last_tooltip,
        })
    }

    fn refresh(
        &mut self,
        lifecycle: ServiceLifecycle,
        status: Option<&StatusSnapshot>,
        notice: &str,
    ) {
        let heart = device_summary(status, DeviceKind::Polar, lifecycle);
        let breath = device_summary(status, DeviceKind::GoDirect, lifecycle);
        let recording = recording_summary(status);
        let service = service_summary(lifecycle, status);
        set_menu_text(&self.service_status, &format!("Service: {service}"));
        set_menu_text(&self.heart_status, &format!("♥ Polar H10: {}", heart.text));
        set_menu_text(
            &self.breath_status,
            &format!("≋ Go Direct: {}", breath.text),
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
            "newKasina · Heart: {} · Breath: {} · {}",
            short_state(heart.light),
            short_state(breath.light),
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
    let bridge = ServiceBridge::default();
    let mut runner = ServiceRunner::new(args, bridge.clone());
    runner.start();

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |event| {
        let _ignored = proxy.send_event(UserEvent::Menu(event));
    }));
    let mut ui: Option<TrayUi> = None;
    let mut quit_when_stopped = false;

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
            Event::MainEventsCleared => {
                runner.poll();
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

fn icon_rgba(state: VisualState) -> Vec<u8> {
    let mut pixels = vec![0_u8; (ICON_SIZE * ICON_SIZE * 4) as usize];
    draw_rounded_background(&mut pixels);
    draw_heart(&mut pixels);
    draw_breath(&mut pixels);
    draw_status_light(&mut pixels, 49, 18, state.heart);
    draw_status_light(&mut pixels, 49, 46, state.breath);
    pixels
}

fn draw_rounded_background(pixels: &mut [u8]) {
    for y in 3..61 {
        for x in 3..61 {
            let corner_x = if x < 13 {
                13 - x
            } else if x > 50 {
                x - 50
            } else {
                0
            };
            let corner_y = if y < 13 {
                13 - y
            } else if y > 50 {
                y - 50
            } else {
                0
            };
            if corner_x * corner_x + corner_y * corner_y <= 100 {
                put_pixel(pixels, x, y, [12, 20, 33, 244]);
            }
        }
    }
}

fn draw_heart(pixels: &mut [u8]) {
    let color = [247, 139, 158, 255];
    for y in 8..33 {
        for x in 7..33 {
            let left_circle = squared_distance(x, y, 14, 15) <= 42;
            let right_circle = squared_distance(x, y, 24, 15) <= 42;
            let triangle = (14..=31).contains(&y) && (x - 19).abs() <= 17 - (y - 14);
            if left_circle || right_circle || triangle {
                put_pixel(pixels, x, y, color);
            }
        }
    }
}

fn draw_breath(pixels: &mut [u8]) {
    const WAVE: [i32; 16] = [0, 1, 2, 3, 3, 2, 1, 0, 0, -1, -2, -3, -3, -2, -1, 0];
    let color = [144, 215, 247, 255];
    for x in 7..34 {
        let offset = WAVE[((x - 7) % WAVE.len() as i32) as usize];
        draw_circle(pixels, x, 42 + offset, 1, color);
        draw_circle(pixels, x, 51 + offset, 1, color);
    }
}

fn draw_status_light(pixels: &mut [u8], center_x: i32, center_y: i32, state: TrafficState) {
    draw_circle(pixels, center_x, center_y, 9, [220, 228, 236, 255]);
    let color = match state {
        TrafficState::Off => [91, 103, 120, 255],
        TrafficState::Connecting => [247, 184, 48, 255],
        TrafficState::Connected => [45, 215, 108, 255],
        TrafficState::Error => [239, 67, 75, 255],
    };
    draw_circle(pixels, center_x, center_y, 7, color);
    draw_circle(pixels, center_x - 2, center_y - 2, 2, [255, 255, 255, 150]);
}

fn draw_circle(pixels: &mut [u8], center_x: i32, center_y: i32, radius: i32, color: [u8; 4]) {
    for y in center_y - radius..=center_y + radius {
        for x in center_x - radius..=center_x + radius {
            if squared_distance(x, y, center_x, center_y) <= radius * radius {
                put_pixel(pixels, x, y, color);
            }
        }
    }
}

fn squared_distance(x: i32, y: i32, center_x: i32, center_y: i32) -> i32 {
    (x - center_x).pow(2) + (y - center_y).pow(2)
}

fn put_pixel(pixels: &mut [u8], x: i32, y: i32, color: [u8; 4]) {
    if x < 0 || y < 0 || x >= ICON_SIZE as i32 || y >= ICON_SIZE as i32 {
        return;
    }
    let index = ((y as u32 * ICON_SIZE + x as u32) * 4) as usize;
    pixels[index..index + 4].copy_from_slice(&color);
}

#[cfg(test)]
mod tests {
    use kasina_protocol::v1::{DeviceInfo, DeviceStatus, RecordingStatus};

    use super::*;

    #[test]
    fn icon_contains_distinct_heart_and_breath_traffic_lights() {
        let icon = icon_rgba(VisualState {
            heart: TrafficState::Connected,
            breath: TrafficState::Error,
        });
        assert_eq!(icon.len(), (ICON_SIZE * ICON_SIZE * 4) as usize);
        assert_eq!(&icon[0..4], &[0, 0, 0, 0]);
        assert_eq!(pixel(&icon, 49, 18), [45, 215, 108, 255]);
        assert_eq!(pixel(&icon, 49, 46), [239, 67, 75, 255]);
        assert_eq!(pixel(&icon, 47, 16), [255, 255, 255, 150]);
        assert_eq!(pixel(&icon, 18, 18), [247, 139, 158, 255]);
        assert_ne!(pixel(&icon, 18, 42)[3], 0);
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
