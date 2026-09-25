//! The acquisition host shared by the standalone service and desktop application.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::ValueEnum;
use kasina_devices::{DriverConnectionState, SensorDriver, SimulatedDriver};
use kasina_godirect::GoDirectDriver;
use kasina_polar::PolarDriver;
use kasina_thoughtstream::{ThoughtStreamDriver, ThoughtStreamPortSelection};
use parking_lot::Mutex;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
    DEFAULT_PORT, DeviceControl, KasinaRpc, PortControl, ServiceLock, ServicePaths, ServiceState,
    bind_loopback, load_or_create_token, port_settings, serve, write_diagnostics_jsonl,
};

/// Sources to supervise during an acquisition lifetime.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum ServiceSource {
    /// Deterministic sample producer for development and demonstrations.
    Simulated,
    /// Only the Polar Bluetooth sensor.
    Polar,
    /// Only the Go Direct Bluetooth sensor.
    GoDirect,
    /// Only the ThoughtStream USB sensor.
    Thoughtstream,
    /// Independently supervise all supported physical sensors.
    #[default]
    Hardware,
}

/// Configuration independent of command-line parsing or native window lifecycles.
#[derive(Debug, Clone)]
pub struct ServiceOptions {
    /// Loopback port; zero requests an ephemeral port for tests.
    pub port: u16,
    /// History retained in memory for reconnecting clients.
    pub retention_seconds: u64,
    /// Override the standard authentication token file.
    pub token_path: Option<PathBuf>,
    /// Override the per-user singleton lock, normally only for isolated tests.
    pub lock_path: Option<PathBuf>,
    /// Override the private raw recording directory.
    pub recordings_dir: Option<PathBuf>,
    /// Acquisition sources; embedded usage defaults to physical hardware.
    pub source: ServiceSource,
    /// Optional preferred Polar peripheral identifier.
    pub polar_id: Option<String>,
    /// Optional preferred Go Direct peripheral identifier.
    pub go_direct_id: Option<String>,
    /// Explicit serial port; otherwise load the saved preference or discover automatically.
    pub thoughtstream_port: Option<String>,
    /// Shared selection for the standalone tray host.
    pub thoughtstream_selection: Option<ThoughtStreamPortSelection>,
    /// Override the device preference file, normally only for isolated tests.
    pub device_settings_path: Option<PathBuf>,
    /// Optional append-only diagnostic output.
    pub diagnostics_jsonl: Option<PathBuf>,
    /// Diagnostic output interval in seconds.
    pub diagnostics_interval_seconds: u64,
}

impl Default for ServiceOptions {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            retention_seconds: 600,
            token_path: None,
            lock_path: None,
            recordings_dir: None,
            source: ServiceSource::Hardware,
            polar_id: None,
            go_direct_id: None,
            thoughtstream_port: None,
            thoughtstream_selection: None,
            device_settings_path: None,
            diagnostics_jsonl: None,
            diagnostics_interval_seconds: 10,
        }
    }
}

#[derive(Debug, Clone)]
enum DriverFactory {
    Simulated,
    Polar(Option<String>),
    GoDirect(Option<String>),
    Thoughtstream(ThoughtStreamPortSelection),
    #[cfg(test)]
    Fixture(&'static str, kasina_domain::StreamKind),
}

impl DriverFactory {
    fn create(&self) -> Box<dyn SensorDriver> {
        match self {
            Self::Simulated => Box::new(SimulatedDriver::default()),
            Self::Polar(id) => Box::new(
                id.as_deref()
                    .map_or_else(PolarDriver::default, PolarDriver::with_target_id),
            ),
            Self::GoDirect(id) => Box::new(
                id.as_deref()
                    .map_or_else(GoDirectDriver::default, GoDirectDriver::with_target_id),
            ),
            Self::Thoughtstream(selection) => {
                Box::new(ThoughtStreamDriver::with_selection(selection.clone()))
            }
            #[cfg(test)]
            Self::Fixture(id, stream) => Box::new(tests::FixtureDriver {
                id,
                stream: *stream,
            }),
        }
    }
}

/// A bound, authenticated acquisition host which has not opened any sensor yet.
///
/// Keep this value, then await [`Self::run`] on a Tokio runtime. Cancel its token and
/// wait for completion before disposing the runtime: completion includes closing
/// sensors, draining recordings, and releasing the singleton lock. No process-wide
/// signal handler is installed by this API.
#[derive(Debug)]
pub struct PreparedService {
    state: Arc<ServiceState>,
    lock: ServiceLock,
    listener: TcpListener,
    address: SocketAddr,
    token: String,
    recordings_dir: PathBuf,
    drivers: Vec<DriverFactory>,
    diagnostics_jsonl: Option<PathBuf>,
    diagnostics_interval: Duration,
}

impl PreparedService {
    /// Reserve ownership and bind before any hardware access or recording recovery.
    pub async fn prepare(options: ServiceOptions) -> Result<Self> {
        anyhow::ensure!(
            options.retention_seconds > 0,
            "retention must be greater than zero"
        );
        anyhow::ensure!(
            options.diagnostics_interval_seconds > 0,
            "diagnostics interval must be greater than zero"
        );
        let defaults = ServicePaths::for_user()?;
        let token_path = options.token_path.unwrap_or(defaults.token);
        let lock_path = options.lock_path.unwrap_or(defaults.lock);
        let recordings_dir = options.recordings_dir.unwrap_or(defaults.recordings);
        let lock = ServiceLock::acquire(&lock_path)?;
        let listener = bind_loopback(options.port).await?;
        let address = listener.local_addr()?;
        let token = load_or_create_token(&token_path)?;
        let settings_path = options
            .device_settings_path
            .map_or_else(port_settings::settings_path, Ok)?;
        let selection = options.thoughtstream_selection.unwrap_or_else(|| {
            let port = options.thoughtstream_port.or_else(|| match port_settings::load(&settings_path) {
                Ok(port) => port,
                Err(error) => {
                    tracing::warn!(%error, "could not load saved ThoughtStream port; using automatic discovery");
                    None
                }
            });
            ThoughtStreamPortSelection::new(port)
        });
        let drivers = match options.source {
            ServiceSource::Simulated => vec![DriverFactory::Simulated],
            ServiceSource::Polar => vec![DriverFactory::Polar(options.polar_id)],
            ServiceSource::GoDirect => vec![DriverFactory::GoDirect(options.go_direct_id)],
            ServiceSource::Thoughtstream => vec![DriverFactory::Thoughtstream(selection.clone())],
            ServiceSource::Hardware => vec![
                DriverFactory::Polar(options.polar_id),
                DriverFactory::GoDirect(options.go_direct_id),
                DriverFactory::Thoughtstream(selection.clone()),
            ],
        };
        let descriptors = drivers.iter().map(|factory| factory.create().descriptor());
        let state = ServiceState::new_multi_with_recordings(
            Duration::from_secs(options.retention_seconds),
            descriptors,
            recordings_dir.clone(),
        )?;
        if matches!(
            options.source,
            ServiceSource::Thoughtstream | ServiceSource::Hardware
        ) {
            *state.port_control.write() = Some(PortControl {
                selection,
                settings_path,
                update: Arc::new(Mutex::new(())),
            });
        }
        tracing::info!(%address, token_path = %token_path.display(), recordings_dir = %recordings_dir.display(), "kasina-service ready");
        Ok(Self {
            state,
            lock,
            listener,
            address,
            token,
            recordings_dir,
            drivers,
            diagnostics_jsonl: options.diagnostics_jsonl,
            diagnostics_interval: Duration::from_secs(options.diagnostics_interval_seconds),
        })
    }

    /// Shared status and controls for hosts that need in-process access.
    #[must_use]
    pub fn state(&self) -> Arc<ServiceState> {
        Arc::clone(&self.state)
    }

    /// Reserved loopback endpoint.
    #[must_use]
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// Actual recording directory after resolving overrides.
    #[must_use]
    pub fn recordings_dir(&self) -> &Path {
        &self.recordings_dir
    }

    /// Start acquisition and authenticated RPC, then fully clean up after cancellation.
    pub async fn run(self, cancellation: CancellationToken) -> Result<()> {
        let Self {
            state,
            lock,
            listener,
            token,
            drivers,
            diagnostics_jsonl,
            diagnostics_interval,
            ..
        } = self;
        if cancellation.is_cancelled() {
            for device in state.devices.write().values_mut() {
                device.connection_enabled = false;
                device.state = DriverConnectionState::Disconnected;
                device.detail = "stopped".to_owned();
            }
            state
                .recording
                .shutdown()
                .await
                .map_err(anyhow::Error::msg)?;
            drop(lock);
            return Ok(());
        }
        let acquisitions = drivers
            .into_iter()
            .map(|factory| {
                let id = factory.create().descriptor().id;
                let (sender, receiver) = mpsc::channel(8);
                state.controls.write().insert(id.clone(), sender);
                tokio::spawn(supervise_driver(
                    Arc::clone(&state),
                    id,
                    factory,
                    receiver,
                    cancellation.child_token(),
                ))
            })
            .collect::<Vec<_>>();
        let diagnostic_cancel = CancellationToken::new();
        let diagnostics = diagnostics_jsonl.map(|path| {
            tokio::spawn(write_diagnostics_jsonl(
                Arc::clone(&state),
                path,
                diagnostics_interval,
                diagnostic_cancel.child_token(),
            ))
        });
        let server_result = serve(
            listener,
            KasinaRpc::new(Arc::clone(&state), token),
            cancellation.clone(),
        )
        .await;
        cancellation.cancel();
        let mut acquisition_result = Ok(());
        for task in acquisitions {
            let result = task.await.context("acquisition supervisor panicked");
            if acquisition_result.is_ok() {
                acquisition_result = result;
            }
        }
        state.controls.write().clear();
        diagnostic_cancel.cancel();
        let diagnostic_result = if let Some(task) = diagnostics {
            task.await
                .context("diagnostics task panicked")
                .and_then(|result| result)
        } else {
            Ok(())
        };
        let recording_result = state.recording.shutdown().await.map_err(anyhow::Error::msg);
        // External status holders may keep `state` alive. Explicitly stop its writer
        // above before releasing ownership, rather than relying on Arc destruction.
        drop(lock);
        server_result?;
        acquisition_result?;
        diagnostic_result?;
        recording_result
    }
}

async fn supervise_driver(
    state: Arc<ServiceState>,
    id: String,
    factory: DriverFactory,
    mut commands: mpsc::Receiver<DeviceControl>,
    cancellation: CancellationToken,
) {
    let start = || {
        if let Some(device) = state.devices.write().get_mut(&id) {
            device.connection_enabled = true;
            device.state = DriverConnectionState::Connecting;
            device.detail = "connecting".to_owned();
        }
        let stop = cancellation.child_token();
        let task = tokio::spawn(Arc::clone(&state).run_driver(factory.create(), stop.clone()));
        (stop, task)
    };
    let mut active = Some(start());
    loop {
        let command = if let Some((_, task)) = active.as_mut() {
            tokio::select! {
                () = cancellation.cancelled() => break,
                command = commands.recv() => command,
                result = task => {
                    if let Err(error) = result
                        && let Some(device) = state.devices.write().get_mut(&id) {
                        device.state = DriverConnectionState::Disconnected;
                        device.detail = format!("driver task failed: {error}");
                    }
                    active = None;
                    if let Some(device) = state.devices.write().get_mut(&id) {
                        device.connection_enabled = false;
                    }
                    continue;
                }
            }
        } else {
            tokio::select! {
                () = cancellation.cancelled() => break,
                command = commands.recv() => command,
            }
        };
        let Some(command) = command else {
            break;
        };
        if command.enabled {
            if active.is_none() {
                active = Some(start());
            }
        } else {
            if let Some((stop, task)) = active.take() {
                stop.cancel();
                let _ = task.await;
            }
            if let Some(device) = state.devices.write().get_mut(&id) {
                device.connection_enabled = false;
                device.state = DriverConnectionState::Disconnected;
                device.detail = "paused by user".to_owned();
            }
        }
        let _ = command.response.send(Ok(()));
    }
    if let Some((stop, task)) = active {
        stop.cancel();
        let _ = task.await;
    }
    if let Some(device) = state.devices.write().get_mut(&id) {
        device.connection_enabled = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kasina_devices::{DeviceDescriptor, DeviceKind, DriverEvent, send_driver_event};
    use kasina_domain::StreamKind;

    #[derive(Debug)]
    pub(super) struct FixtureDriver {
        pub id: &'static str,
        pub stream: StreamKind,
    }

    #[async_trait::async_trait]
    impl SensorDriver for FixtureDriver {
        fn descriptor(&self) -> DeviceDescriptor {
            DeviceDescriptor {
                id: self.id.to_owned(),
                name: self.id.to_owned(),
                kind: DeviceKind::Simulated,
            }
        }

        async fn run(
            self: Box<Self>,
            sender: mpsc::Sender<DriverEvent>,
            cancellation: CancellationToken,
        ) -> Result<()> {
            let mut ticker = tokio::time::interval(Duration::from_millis(10));
            loop {
                tokio::select! {
                    () = cancellation.cancelled() => return Ok(()),
                    _ = ticker.tick() => {
                        if !send_driver_event(&sender, &cancellation, DriverEvent::Measurement {
                            stream: self.stream, source_id: self.id.to_owned(), device_time_ns: None,
                            value: 1.0, quality_flags: 0,
                        }).await? { return Ok(()); }
                    }
                }
            }
        }
    }

    fn newest(state: &ServiceState, stream: StreamKind) -> u64 {
        state
            .status_snapshot()
            .streams
            .into_iter()
            .find(|status| status.stream == crate::proto_stream(stream) as i32)
            .map_or(0, |status| status.newest_sequence)
    }

    async fn wait_until_newer(state: &ServiceState, stream: StreamKind, previous: u64) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while newest(state, stream) <= previous {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn stopping_one_driver_preserves_the_other_and_resumes_service_sequences() {
        let factories = [
            DriverFactory::Fixture("first", StreamKind::HeartRate),
            DriverFactory::Fixture("second", StreamKind::RespirationForce),
        ];
        let state = ServiceState::new_multi(
            Duration::from_secs(30),
            factories
                .iter()
                .map(|factory| factory.create().descriptor()),
        );
        let cancellation = CancellationToken::new();
        let mut tasks = Vec::new();
        for factory in factories {
            let id = factory.create().descriptor().id;
            let (sender, receiver) = mpsc::channel(8);
            state.controls.write().insert(id.clone(), sender);
            tasks.push(tokio::spawn(supervise_driver(
                Arc::clone(&state),
                id,
                factory,
                receiver,
                cancellation.child_token(),
            )));
        }
        wait_until_newer(&state, StreamKind::HeartRate, 0).await;
        wait_until_newer(&state, StreamKind::RespirationForce, 0).await;
        state.set_device_connection("first", false).await.unwrap();
        let first = newest(&state, StreamKind::HeartRate);
        let second = newest(&state, StreamKind::RespirationForce);
        wait_until_newer(&state, StreamKind::RespirationForce, second + 3).await;
        assert_eq!(newest(&state, StreamKind::HeartRate), first);
        assert!(!state.status_snapshot().devices[0].connection_enabled);
        state.set_device_connection("first", true).await.unwrap();
        wait_until_newer(&state, StreamKind::HeartRate, first).await;
        cancellation.cancel();
        for task in tasks {
            tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .unwrap()
                .unwrap();
        }
    }

    #[tokio::test]
    async fn unsupervised_state_rejects_connection_commands() {
        let descriptor = SimulatedDriver::default().descriptor();
        let state = ServiceState::new(Duration::from_secs(30), descriptor.clone());
        assert!(!state.status_snapshot().devices[0].connection_control_available);
        let error = state
            .set_device_connection(&descriptor.id, false)
            .await
            .unwrap_err();
        assert_eq!(error.code(), tonic::Code::FailedPrecondition);
    }
}
