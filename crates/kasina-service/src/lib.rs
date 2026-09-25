//! Persistent acquisition state and authenticated streaming RPC implementation.

mod recording;

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use async_stream::try_stream;
use fs2::FileExt;
use futures::Stream;
use kasina_devices::{
    DeviceDescriptor, DeviceKind, DriverConnectionState, DriverEvent, SensorDriver,
};
use kasina_domain::{Sample, ServiceBuffers, StreamKind};
use kasina_protocol::v1::kasina_server::{Kasina, KasinaServer};
use kasina_protocol::v1::{
    ClientHello, ConnectionState, DeviceCommand, DeviceInfo, DeviceList, DeviceStatus,
    PreferredDeviceCommand, RecordingList, RecordingState, RecordingStatus, SampleBatch,
    SamplesSinceRequest, ServiceInfo, StartRecordingRequest, StatusSnapshot, StopRecordingRequest,
    StreamDiagnostics, StreamGap, SubscribeRequest,
};
use kasina_protocol::{AUTH_HEADER, PROTOCOL_MAJOR, PROTOCOL_MINOR};
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::TcpListenerStream;
use tokio_util::sync::CancellationToken;
use tonic::transport::Server;
use tonic::{Request, Response, Status};
use uuid::Uuid;

use recording::{RecordingManager, RecordingSnapshot, SessionState};

/// Default local service port, retained from the Python implementation.
pub const DEFAULT_PORT: u16 = 18_861;
/// Short transport batching interval.
pub const BATCH_INTERVAL: Duration = Duration::from_millis(20);
const SAMPLE_BROADCAST_CAPACITY: usize = 4_096;

/// Append-only service health record suitable for long hardware soaks.
#[derive(Debug, Clone, Serialize)]
pub struct ServiceDiagnosticRecord {
    /// Schema revision for future compatible readers.
    pub schema_version: u32,
    /// Wall-clock time when the snapshot was taken.
    pub wall_time_unix_ns: u64,
    /// Unique service process instance.
    pub service_instance_id: String,
    /// Service uptime.
    pub uptime_millis: u64,
    /// Current per-device status.
    pub devices: Vec<DeviceDiagnosticRecord>,
    /// Current per-stream retention and loss status.
    pub streams: Vec<StreamDiagnosticRecord>,
    /// Number of connected RPC clients.
    pub connected_clients: u64,
    /// Samples skipped because a live transport subscriber lagged.
    pub transport_lagged_samples: u64,
}

/// Serializable device subsection of a soak record.
#[derive(Debug, Clone, Serialize)]
pub struct DeviceDiagnosticRecord {
    /// Stable configured device identifier.
    pub id: String,
    /// User-visible driver name.
    pub name: String,
    /// Protocol device kind name.
    pub kind: String,
    /// Protocol connection state name.
    pub state: String,
    /// Driver-provided diagnostic detail.
    pub detail: String,
    /// Number of supervised reconnect attempts.
    pub reconnect_attempts: u64,
    /// Age of the newest sample from this source.
    pub last_sample_age_millis: u64,
}

/// Serializable stream subsection of a soak record.
#[derive(Debug, Clone, Serialize)]
pub struct StreamDiagnosticRecord {
    /// Protocol stream kind name.
    pub stream: String,
    /// Newest service-assigned sequence.
    pub newest_sequence: u64,
    /// Samples currently retained in memory.
    pub retained_samples: u64,
    /// Retained time span.
    pub retained_duration_millis: u64,
    /// Samples evicted from bounded retention.
    pub dropped_samples: u64,
    /// Age of the newest sample.
    pub last_sample_age_millis: u64,
}

/// Paths used by the per-user service.
#[derive(Debug, Clone)]
pub struct ServicePaths {
    /// Random authentication token file.
    pub token: PathBuf,
    /// Single-instance lock file.
    pub lock: PathBuf,
    /// Private, analysis-ready raw session recordings.
    pub recordings: PathBuf,
}

impl ServicePaths {
    /// Resolve standard per-user paths for the current operating system.
    pub fn for_user() -> Result<Self> {
        let project = directories::ProjectDirs::from("org", "newkasina", "newKasina")
            .context("operating system did not provide a user configuration directory")?;
        let config = project.config_dir();
        Ok(Self {
            token: config.join("service-token"),
            lock: config.join("service.lock"),
            recordings: project.data_local_dir().join("sessions"),
        })
    }
}

/// Hold this value for the lifetime of the service to enforce a single instance.
#[derive(Debug)]
pub struct ServiceLock {
    file: File,
}

impl ServiceLock {
    /// Acquire the per-user service lock without blocking.
    pub fn acquire(path: &Path) -> Result<Self> {
        ensure_parent(path)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("open service lock {}", path.display()))?;
        FileExt::try_lock_exclusive(&file)
            .with_context(|| format!("another kasina-service owns {}", path.display()))?;
        Ok(Self { file })
    }
}

impl Drop for ServiceLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

fn ensure_parent(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create configuration directory {}", parent.display()))?;
    Ok(())
}

/// Load an existing token or atomically create a new random one.
pub fn load_or_create_token(path: &Path) -> Result<String> {
    ensure_parent(path)?;
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => {
            let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
            file.write_all(token.as_bytes())?;
            file.sync_all()?;
            restrict_user_permissions(path)?;
            Ok(token)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => read_token(path),
        Err(error) => Err(error).with_context(|| format!("create token {}", path.display())),
    }
}

/// Read the service token used by clients.
pub fn read_token(path: &Path) -> Result<String> {
    let mut token = String::new();
    File::open(path)
        .with_context(|| format!("open token {}", path.display()))?
        .read_to_string(&mut token)?;
    let token = token.trim().to_owned();
    anyhow::ensure!(!token.is_empty(), "service token is empty");
    Ok(token)
}

#[cfg(unix)]
fn restrict_user_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_user_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[derive(Debug, Clone)]
struct DeviceRuntime {
    descriptor: DeviceDescriptor,
    active_source_id: String,
    state: DriverConnectionState,
    detail: String,
    reconnect_attempts: u64,
}

/// Shared acquisition state, independent of transport connections.
#[derive(Debug)]
pub struct ServiceState {
    started: Instant,
    instance_id: String,
    buffers: RwLock<ServiceBuffers>,
    sequences: Mutex<std::collections::BTreeMap<StreamKind, u64>>,
    sample_sender: broadcast::Sender<Sample>,
    devices: RwLock<std::collections::BTreeMap<String, DeviceRuntime>>,
    connected_clients: AtomicU64,
    transport_lagged_samples: AtomicU64,
    service_batch_sequence: AtomicU64,
    recording: RecordingManager,
}

impl ServiceState {
    /// Create isolated persistent state for one service instance.
    #[must_use]
    pub fn new(retention: Duration, device: DeviceDescriptor) -> Arc<Self> {
        Self::new_multi(retention, [device])
    }

    /// Create service state for multiple independently supervised devices.
    #[must_use]
    pub fn new_multi(
        retention: Duration,
        devices: impl IntoIterator<Item = DeviceDescriptor>,
    ) -> Arc<Self> {
        Self::build(retention, devices, RecordingManager::unavailable())
    }

    /// Create service state with private persistent recording storage.
    pub fn new_multi_with_recordings(
        retention: Duration,
        devices: impl IntoIterator<Item = DeviceDescriptor>,
        recordings: PathBuf,
    ) -> Result<Arc<Self>> {
        Ok(Self::build(
            retention,
            devices,
            RecordingManager::new(recordings)?,
        ))
    }

    fn build(
        retention: Duration,
        devices: impl IntoIterator<Item = DeviceDescriptor>,
        recording: RecordingManager,
    ) -> Arc<Self> {
        let (sample_sender, _) = broadcast::channel(SAMPLE_BROADCAST_CAPACITY);
        let devices = devices
            .into_iter()
            .map(|descriptor| {
                let id = descriptor.id.clone();
                (
                    id.clone(),
                    DeviceRuntime {
                        descriptor,
                        active_source_id: id,
                        state: DriverConnectionState::Connecting,
                        detail: "starting".to_owned(),
                        reconnect_attempts: 0,
                    },
                )
            })
            .collect();
        Arc::new(Self {
            started: Instant::now(),
            instance_id: Uuid::new_v4().to_string(),
            buffers: RwLock::new(ServiceBuffers::new(retention)),
            sequences: Mutex::new(std::collections::BTreeMap::new()),
            sample_sender,
            devices: RwLock::new(devices),
            connected_clients: AtomicU64::new(0),
            transport_lagged_samples: AtomicU64::new(0),
            service_batch_sequence: AtomicU64::new(0),
            recording,
        })
    }

    /// Consume events from one driver until it ends or is cancelled.
    pub async fn run_driver(
        self: Arc<Self>,
        driver: Box<dyn SensorDriver>,
        cancellation: CancellationToken,
    ) -> Result<()> {
        let runtime_id = driver.descriptor().id;
        let (sender, mut receiver) = mpsc::channel(1_024);
        let driver_cancel = cancellation.child_token();
        let driver_task = tokio::spawn(driver.run(sender, driver_cancel.clone()));

        while let Some(event) = receiver.recv().await {
            match event {
                DriverEvent::Measurement {
                    stream,
                    source_id,
                    device_time_ns,
                    value,
                    quality_flags,
                } => {
                    if let Some(device) = self.devices.write().get_mut(&runtime_id) {
                        device.active_source_id.clone_from(&source_id);
                    }
                    let sequence = {
                        let mut sequences = self.sequences.lock();
                        let next = sequences.entry(stream).or_insert(0);
                        *next = next.saturating_add(1);
                        *next
                    };
                    let mut sample = Sample::new(
                        stream,
                        source_id,
                        sequence,
                        self.started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64,
                        unix_time_ns(),
                        value,
                    );
                    sample.device_time_ns = device_time_ns;
                    sample.quality_flags = quality_flags;
                    self.buffers
                        .write()
                        .push(sample.clone())
                        .context("service buffer rejected driver sample")?;
                    self.recording.record(sample.clone());
                    let _ = self.sample_sender.send(sample);
                }
                DriverEvent::Status {
                    source_id,
                    state,
                    detail,
                } => {
                    if let Some(device) = self.devices.write().get_mut(&runtime_id) {
                        device.active_source_id = source_id;
                        device.state = state;
                        if state == DriverConnectionState::Reconnecting {
                            device.reconnect_attempts = device.reconnect_attempts.saturating_add(1);
                        }
                        device.detail = detail;
                    }
                }
            }
        }

        driver_cancel.cancel();
        let driver_result = driver_task.await.context("driver task panicked")?;
        if let Some(device) = self.devices.write().get_mut(&runtime_id) {
            device.state = DriverConnectionState::Disconnected;
            device.detail = match (&driver_result, cancellation.is_cancelled()) {
                (_, true) => "stopped".to_owned(),
                (Ok(()), false) => "driver finished".to_owned(),
                (Err(error), false) => format!("driver failed: {error:#}"),
            };
        }
        driver_result
    }

    /// Return the current device, stream, client, and recording status.
    #[must_use]
    pub fn status_snapshot(&self) -> StatusSnapshot {
        let uptime = self.started.elapsed();
        let now_ns = uptime.as_nanos().min(u128::from(u64::MAX)) as u64;
        let buffers = self.buffers.read();
        let streams = buffers
            .iter()
            .map(|(kind, buffer)| {
                let newest = buffer.newest();
                StreamDiagnostics {
                    stream: proto_stream(kind) as i32,
                    newest_sequence: newest.map_or(0, |sample| sample.sequence),
                    retained_samples: buffer.len() as u64,
                    retained_duration_millis: buffer.retained_duration().as_millis() as u64,
                    dropped_samples: buffer.evicted_samples(),
                    last_sample_age_millis: newest.map_or(0, |sample| {
                        now_ns.saturating_sub(sample.monotonic_time_ns) / 1_000_000
                    }),
                }
            })
            .collect();
        drop(buffers);
        let devices = self
            .devices
            .read()
            .values()
            .map(|runtime| self.proto_device_status(runtime, now_ns))
            .collect();
        StatusSnapshot {
            uptime_millis: uptime.as_millis() as u64,
            devices,
            streams,
            connected_clients: self.connected_clients.load(Ordering::Relaxed),
            transport_lagged_samples: self.transport_lagged_samples.load(Ordering::Relaxed),
            recording: Some(proto_recording(self.recording.snapshot())),
        }
    }

    /// Start a service-owned recording without going through the RPC transport.
    ///
    /// This is used by the system-tray host running in the same process. Remote callers
    /// still pass through the authenticated RPC implementation.
    pub async fn start_recording(
        &self,
        label: String,
        notes: String,
    ) -> std::result::Result<RecordingStatus, String> {
        if label.trim().is_empty() {
            return Err("recording label cannot be empty".to_owned());
        }
        let devices = self
            .devices
            .read()
            .values()
            .map(|runtime| runtime.descriptor.clone())
            .collect();
        self.recording
            .start(label, notes, self.instance_id.clone(), devices)
            .await
            .map(proto_recording)
    }

    /// Stop and durably finalize the active service-owned recording.
    pub async fn stop_recording(&self) -> std::result::Result<RecordingStatus, String> {
        self.recording.stop().await.map(proto_recording)
    }

    /// Produce a stable serializable health snapshot for soak-test logging.
    #[must_use]
    pub fn diagnostic_record(&self) -> ServiceDiagnosticRecord {
        let status = self.status_snapshot();
        ServiceDiagnosticRecord {
            schema_version: 1,
            wall_time_unix_ns: unix_time_ns(),
            service_instance_id: self.instance_id.clone(),
            uptime_millis: status.uptime_millis,
            devices: status
                .devices
                .into_iter()
                .map(|status| {
                    let device = status.device.unwrap_or_default();
                    DeviceDiagnosticRecord {
                        id: device.id,
                        name: device.name,
                        kind: kasina_protocol::v1::DeviceKind::try_from(device.kind)
                            .map_or("DEVICE_KIND_UNSPECIFIED", |kind| kind.as_str_name())
                            .to_owned(),
                        state: ConnectionState::try_from(status.state)
                            .map_or("CONNECTION_STATE_UNSPECIFIED", |state| state.as_str_name())
                            .to_owned(),
                        detail: status.detail,
                        reconnect_attempts: status.reconnect_attempts,
                        last_sample_age_millis: status.last_sample_age_millis,
                    }
                })
                .collect(),
            streams: status
                .streams
                .into_iter()
                .map(|stream| StreamDiagnosticRecord {
                    stream: kasina_protocol::v1::StreamKind::try_from(stream.stream)
                        .map_or("STREAM_KIND_UNSPECIFIED", |kind| kind.as_str_name())
                        .to_owned(),
                    newest_sequence: stream.newest_sequence,
                    retained_samples: stream.retained_samples,
                    retained_duration_millis: stream.retained_duration_millis,
                    dropped_samples: stream.dropped_samples,
                    last_sample_age_millis: stream.last_sample_age_millis,
                })
                .collect(),
            connected_clients: status.connected_clients,
            transport_lagged_samples: status.transport_lagged_samples,
        }
    }

    fn proto_device_status(&self, runtime: &DeviceRuntime, now_ns: u64) -> DeviceStatus {
        let last_sample_time = self
            .buffers
            .read()
            .iter()
            .filter_map(|(_, buffer)| buffer.newest())
            .filter(|sample| sample.source_id == runtime.active_source_id)
            .map(|sample| sample.monotonic_time_ns)
            .max();
        DeviceStatus {
            device: Some(DeviceInfo {
                id: runtime.descriptor.id.clone(),
                name: runtime.descriptor.name.clone(),
                kind: proto_device_kind(runtime.descriptor.kind) as i32,
                preferred: true,
            }),
            state: match runtime.state {
                DriverConnectionState::Connecting => ConnectionState::Connecting,
                DriverConnectionState::Connected => ConnectionState::Connected,
                DriverConnectionState::Reconnecting => ConnectionState::Reconnecting,
                DriverConnectionState::Disconnected => ConnectionState::Disconnected,
            } as i32,
            detail: runtime.detail.clone(),
            reconnect_attempts: runtime.reconnect_attempts,
            last_sample_age_millis: last_sample_time
                .map_or(0, |time| now_ns.saturating_sub(time) / 1_000_000),
        }
    }
}

/// Periodically append service health snapshots until cancellation.
pub async fn write_diagnostics_jsonl(
    state: Arc<ServiceState>,
    path: PathBuf,
    interval: Duration,
    cancellation: CancellationToken,
) -> Result<()> {
    if interval.is_zero() {
        anyhow::bail!("diagnostics interval must be greater than zero");
    }
    ensure_parent(&path)?;
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            () = cancellation.cancelled() => break,
            _ = ticker.tick() => {
                append_diagnostic_snapshot(&state, &path).await?;
            }
        }
    }
    append_diagnostic_snapshot(&state, &path).await
}

async fn append_diagnostic_snapshot(state: &ServiceState, path: &Path) -> Result<()> {
    let record = state.diagnostic_record();
    let output = path.to_owned();
    tokio::task::spawn_blocking(move || append_diagnostic_record(&output, &record))
        .await
        .context("diagnostics writer task panicked")?
}

fn append_diagnostic_record(path: &Path, record: &ServiceDiagnosticRecord) -> Result<()> {
    let mut file = open_private_append(path)
        .with_context(|| format!("open diagnostics file {}", path.display()))?;
    restrict_user_permissions(path)?;
    serde_json::to_writer(&mut file, record).context("serialize service diagnostics")?;
    file.write_all(b"\n")?;
    file.flush()?;
    Ok(())
}

fn open_private_append(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn unix_time_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_nanos()
        .min(u128::from(u64::MAX)) as u64
}

fn proto_stream(stream: StreamKind) -> kasina_protocol::v1::StreamKind {
    use kasina_protocol::v1::StreamKind as Proto;
    match stream {
        StreamKind::HeartRate => Proto::HeartRate,
        StreamKind::RrInterval => Proto::RrInterval,
        StreamKind::RespirationForce => Proto::RespirationForce,
        StreamKind::AccelerationX => Proto::AccelerationX,
        StreamKind::AccelerationY => Proto::AccelerationY,
        StreamKind::AccelerationZ => Proto::AccelerationZ,
        StreamKind::SkinResistance => Proto::SkinResistance,
        StreamKind::ThoughtStreamAdc => Proto::ThoughtStreamAdc,
    }
}

fn domain_stream(stream: i32) -> Result<StreamKind, Status> {
    use kasina_protocol::v1::StreamKind as Proto;
    match Proto::try_from(stream).map_err(|_| Status::invalid_argument("unknown stream"))? {
        Proto::HeartRate => Ok(StreamKind::HeartRate),
        Proto::RrInterval => Ok(StreamKind::RrInterval),
        Proto::RespirationForce => Ok(StreamKind::RespirationForce),
        Proto::AccelerationX => Ok(StreamKind::AccelerationX),
        Proto::AccelerationY => Ok(StreamKind::AccelerationY),
        Proto::AccelerationZ => Ok(StreamKind::AccelerationZ),
        Proto::SkinResistance => Ok(StreamKind::SkinResistance),
        Proto::ThoughtStreamAdc => Ok(StreamKind::ThoughtStreamAdc),
        Proto::Unspecified => Err(Status::invalid_argument("stream is unspecified")),
    }
}

fn proto_device_kind(kind: DeviceKind) -> kasina_protocol::v1::DeviceKind {
    use kasina_protocol::v1::DeviceKind as Proto;
    match kind {
        DeviceKind::Polar => Proto::Polar,
        DeviceKind::GoDirect => Proto::GoDirect,
        DeviceKind::ThoughtStream => Proto::ThoughtStream,
        DeviceKind::Simulated => Proto::Simulated,
    }
}

fn proto_sample(sample: Sample) -> kasina_protocol::v1::Sample {
    kasina_protocol::v1::Sample {
        stream: proto_stream(sample.stream) as i32,
        source_id: sample.source_id,
        sequence: sample.sequence,
        monotonic_time_ns: sample.monotonic_time_ns,
        wall_time_unix_ns: sample.wall_time_unix_ns,
        device_time_ns: sample.device_time_ns,
        value: sample.value,
        unit: sample.unit,
        quality_flags: sample.quality_flags,
    }
}

fn proto_recording(snapshot: RecordingSnapshot) -> RecordingStatus {
    RecordingStatus {
        state: match snapshot.state {
            SessionState::Unavailable => RecordingState::Unavailable,
            SessionState::Idle => RecordingState::Idle,
            SessionState::Recording => RecordingState::Recording,
            SessionState::Completed => RecordingState::Completed,
            SessionState::Interrupted => RecordingState::Interrupted,
            SessionState::Error => RecordingState::Error,
        } as i32,
        session_id: snapshot.session_id,
        label: snapshot.label,
        directory: snapshot.directory,
        started_wall_time_unix_ns: snapshot.started_wall_time_unix_ns,
        stopped_wall_time_unix_ns: snapshot.stopped_wall_time_unix_ns,
        sample_count: snapshot.sample_count,
        dropped_samples: snapshot.dropped_samples,
        detail: snapshot.detail,
    }
}

/// Authenticated tonic implementation backed by [`ServiceState`].
#[derive(Debug, Clone)]
pub struct KasinaRpc {
    state: Arc<ServiceState>,
    token: Arc<str>,
}

impl KasinaRpc {
    /// Construct an RPC facade around persistent state.
    #[must_use]
    pub fn new(state: Arc<ServiceState>, token: impl Into<Arc<str>>) -> Self {
        Self {
            state,
            token: token.into(),
        }
    }

    fn authorize<T>(&self, request: &Request<T>) -> Result<(), Status> {
        let supplied = request
            .metadata()
            .get(AUTH_HEADER)
            .and_then(|value| value.to_str().ok());
        if supplied == Some(self.token.as_ref()) {
            Ok(())
        } else {
            Err(Status::unauthenticated("missing or invalid service token"))
        }
    }

    fn check_hello(hello: Option<&ClientHello>) -> Result<(), Status> {
        let hello = hello.ok_or_else(|| Status::invalid_argument("client hello is required"))?;
        if hello.protocol_major != PROTOCOL_MAJOR {
            return Err(Status::failed_precondition(format!(
                "protocol major mismatch: service={PROTOCOL_MAJOR}, client={}",
                hello.protocol_major
            )));
        }
        Ok(())
    }

    fn now_ns(&self) -> u64 {
        self.state
            .started
            .elapsed()
            .as_nanos()
            .min(u128::from(u64::MAX)) as u64
    }
}

struct ClientGuard(Arc<ServiceState>);

impl ClientGuard {
    fn new(state: Arc<ServiceState>) -> Self {
        state.connected_clients.fetch_add(1, Ordering::Relaxed);
        Self(state)
    }
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.0.connected_clients.fetch_sub(1, Ordering::Relaxed);
    }
}

type RpcStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send + 'static>>;

#[tonic::async_trait]
impl Kasina for KasinaRpc {
    type SubscribeSamplesStream = RpcStream<SampleBatch>;
    type SubscribeStatusStream = RpcStream<StatusSnapshot>;

    async fn get_service_info(
        &self,
        request: Request<ClientHello>,
    ) -> Result<Response<ServiceInfo>, Status> {
        self.authorize(&request)?;
        Self::check_hello(Some(request.get_ref()))?;
        Ok(Response::new(ServiceInfo {
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            service_version: env!("CARGO_PKG_VERSION").to_owned(),
            instance_id: self.state.instance_id.clone(),
            uptime_millis: self.state.started.elapsed().as_millis() as u64,
        }))
    }

    async fn list_devices(
        &self,
        request: Request<ClientHello>,
    ) -> Result<Response<DeviceList>, Status> {
        self.authorize(&request)?;
        Self::check_hello(Some(request.get_ref()))?;
        let now_ns = self
            .state
            .started
            .elapsed()
            .as_nanos()
            .min(u128::from(u64::MAX)) as u64;
        let devices = self
            .state
            .devices
            .read()
            .values()
            .map(|runtime| self.state.proto_device_status(runtime, now_ns))
            .collect();
        Ok(Response::new(DeviceList { devices }))
    }

    async fn set_device_connection(
        &self,
        request: Request<DeviceCommand>,
    ) -> Result<Response<DeviceStatus>, Status> {
        self.authorize(&request)?;
        Self::check_hello(request.get_ref().client.as_ref())?;
        let command = request.into_inner();
        let mut devices = self.state.devices.write();
        let runtime = devices
            .get_mut(&command.device_id)
            .ok_or_else(|| Status::not_found("unknown device"))?;
        runtime.detail = if command.connect {
            "connection requested; driver supervisor owns lifecycle".to_owned()
        } else {
            "disconnect requested; driver supervisor owns lifecycle".to_owned()
        };
        let status = self.state.proto_device_status(runtime, self.now_ns());
        drop(devices);
        Ok(Response::new(status))
    }

    async fn set_preferred_device(
        &self,
        request: Request<PreferredDeviceCommand>,
    ) -> Result<Response<DeviceStatus>, Status> {
        self.authorize(&request)?;
        Self::check_hello(request.get_ref().client.as_ref())?;
        let command = request.into_inner();
        let devices = self.state.devices.read();
        let runtime = devices
            .get(&command.device_id)
            .ok_or_else(|| Status::not_found("unknown device"))?;
        Ok(Response::new(
            self.state.proto_device_status(runtime, self.now_ns()),
        ))
    }

    async fn get_status(
        &self,
        request: Request<ClientHello>,
    ) -> Result<Response<StatusSnapshot>, Status> {
        self.authorize(&request)?;
        Self::check_hello(Some(request.get_ref()))?;
        Ok(Response::new(self.state.status_snapshot()))
    }

    async fn subscribe_status(
        &self,
        request: Request<ClientHello>,
    ) -> Result<Response<Self::SubscribeStatusStream>, Status> {
        self.authorize(&request)?;
        Self::check_hello(Some(request.get_ref()))?;
        let state = Arc::clone(&self.state);
        let output = try_stream! {
            let _guard = ClientGuard::new(Arc::clone(&state));
            let mut ticker = tokio::time::interval(Duration::from_millis(500));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                yield state.status_snapshot();
            }
        };
        Ok(Response::new(Box::pin(output)))
    }

    async fn get_samples_since(
        &self,
        request: Request<SamplesSinceRequest>,
    ) -> Result<Response<SampleBatch>, Status> {
        self.authorize(&request)?;
        Self::check_hello(request.get_ref().client.as_ref())?;
        let request = request.into_inner();
        let buffers = self.state.buffers.read();
        let mut samples = Vec::new();
        let mut gaps = Vec::new();
        for cursor in request.cursors {
            let stream = domain_stream(cursor.stream)?;
            let Some(buffer) = buffers.get(stream) else {
                continue;
            };
            let snapshot = buffer.snapshot_after(cursor.after_sequence);
            if snapshot.gap {
                gaps.push(StreamGap {
                    stream: proto_stream(stream) as i32,
                    requested_after_sequence: cursor.after_sequence,
                    oldest_available_sequence: snapshot.oldest_available_sequence,
                    dropped_samples: snapshot
                        .oldest_available_sequence
                        .saturating_sub(cursor.after_sequence.saturating_add(1)),
                });
            }
            samples.extend(snapshot.samples.into_iter().map(proto_sample));
        }
        samples.sort_by_key(|sample| (sample.monotonic_time_ns, sample.stream, sample.sequence));
        Ok(Response::new(SampleBatch {
            samples,
            gaps,
            service_batch_sequence: self
                .state
                .service_batch_sequence
                .fetch_add(1, Ordering::Relaxed)
                .saturating_add(1),
        }))
    }

    async fn subscribe_samples(
        &self,
        request: Request<SubscribeRequest>,
    ) -> Result<Response<Self::SubscribeSamplesStream>, Status> {
        self.authorize(&request)?;
        Self::check_hello(request.get_ref().client.as_ref())?;
        let requested: HashSet<_> = request
            .get_ref()
            .streams
            .iter()
            .copied()
            .map(domain_stream)
            .collect::<Result<_, _>>()?;
        let state = Arc::clone(&self.state);
        let mut receiver = state.sample_sender.subscribe();
        let output = try_stream! {
            let _guard = ClientGuard::new(Arc::clone(&state));
            let mut pending = Vec::with_capacity(64);
            let mut pending_lag = 0_u64;
            let mut ticker = tokio::time::interval(BATCH_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            ticker.tick().await;
            loop {
                tokio::select! {
                    received = receiver.recv() => match received {
                        Ok(sample) => {
                            if requested.is_empty() || requested.contains(&sample.stream) {
                                pending.push(proto_sample(sample));
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(count)) => {
                            pending_lag = pending_lag.saturating_add(count);
                            state.transport_lagged_samples.fetch_add(count, Ordering::Relaxed);
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    },
                    _ = ticker.tick() => {
                        if pending.is_empty() && pending_lag == 0 {
                            continue;
                        }
                        let gaps = if pending_lag == 0 {
                            Vec::new()
                        } else {
                            vec![StreamGap {
                                stream: kasina_protocol::v1::StreamKind::Unspecified as i32,
                                requested_after_sequence: 0,
                                oldest_available_sequence: 0,
                                dropped_samples: pending_lag,
                            }]
                        };
                        yield SampleBatch {
                            samples: std::mem::take(&mut pending),
                            gaps,
                            service_batch_sequence: state
                                .service_batch_sequence
                                .fetch_add(1, Ordering::Relaxed)
                                .saturating_add(1),
                        };
                        pending_lag = 0;
                    }
                }
            }
        };
        Ok(Response::new(Box::pin(output)))
    }

    async fn start_recording(
        &self,
        request: Request<StartRecordingRequest>,
    ) -> Result<Response<RecordingStatus>, Status> {
        self.authorize(&request)?;
        Self::check_hello(request.get_ref().client.as_ref())?;
        let command = request.into_inner();
        let snapshot = self
            .state
            .start_recording(command.label, command.notes)
            .await
            .map_err(|error| {
                if error == "recording label cannot be empty" {
                    Status::invalid_argument(error)
                } else {
                    Status::failed_precondition(error)
                }
            })?;
        Ok(Response::new(snapshot))
    }

    async fn stop_recording(
        &self,
        request: Request<StopRecordingRequest>,
    ) -> Result<Response<RecordingStatus>, Status> {
        self.authorize(&request)?;
        Self::check_hello(request.get_ref().client.as_ref())?;
        let snapshot = self
            .state
            .stop_recording()
            .await
            .map_err(Status::failed_precondition)?;
        Ok(Response::new(snapshot))
    }

    async fn get_recording_status(
        &self,
        request: Request<ClientHello>,
    ) -> Result<Response<RecordingStatus>, Status> {
        self.authorize(&request)?;
        Self::check_hello(Some(request.get_ref()))?;
        Ok(Response::new(proto_recording(
            self.state.recording.snapshot(),
        )))
    }

    async fn list_recordings(
        &self,
        request: Request<ClientHello>,
    ) -> Result<Response<RecordingList>, Status> {
        self.authorize(&request)?;
        Self::check_hello(Some(request.get_ref()))?;
        let sessions = self
            .state
            .recording
            .list()
            .await
            .map_err(Status::internal)?
            .into_iter()
            .map(proto_recording)
            .collect();
        Ok(Response::new(RecordingList { sessions }))
    }
}

/// Serve RPC until `cancellation` fires. The listener may use an ephemeral test port.
pub async fn serve(
    listener: TcpListener,
    rpc: KasinaRpc,
    cancellation: CancellationToken,
) -> Result<()> {
    Server::builder()
        .add_service(KasinaServer::new(rpc))
        .serve_with_incoming_shutdown(TcpListenerStream::new(listener), cancellation.cancelled())
        .await
        .context("kasina RPC server failed")
}

/// Bind the production loopback endpoint.
pub async fn bind_loopback(port: u16) -> Result<TcpListener> {
    TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port)))
        .await
        .with_context(|| format!("bind kasina-service to 127.0.0.1:{port}"))
}

/// Add the authentication metadata expected by the service.
pub fn authenticated_request<T>(message: T, token: &str) -> Result<Request<T>> {
    let mut request = Request::new(message);
    request.metadata_mut().insert(
        AUTH_HEADER,
        token
            .parse()
            .context("service token is not valid gRPC metadata")?,
    );
    Ok(request)
}

#[cfg(test)]
mod tests {
    use async_stream::stream;
    use futures::StreamExt as _;

    use super::*;
    use kasina_devices::SimulatedDriver;
    use kasina_protocol::client_hello;

    #[derive(Debug)]
    struct FailingDriver;

    #[async_trait::async_trait]
    impl SensorDriver for FailingDriver {
        fn descriptor(&self) -> DeviceDescriptor {
            DeviceDescriptor {
                id: "failing:test".to_owned(),
                name: "Failing test device".to_owned(),
                kind: DeviceKind::Simulated,
            }
        }

        async fn run(
            self: Box<Self>,
            sender: mpsc::Sender<DriverEvent>,
            _cancellation: CancellationToken,
        ) -> Result<()> {
            sender
                .send(DriverEvent::Status {
                    source_id: "failing:test".to_owned(),
                    state: DriverConnectionState::Connected,
                    detail: "connected before failure".to_owned(),
                })
                .await?;
            anyhow::bail!("intentional driver failure")
        }
    }

    #[tokio::test]
    async fn broadcast_backpressure_is_reported_as_an_explicit_gap() {
        let descriptor = SimulatedDriver::default().descriptor();
        let state = ServiceState::new(Duration::from_secs(30), descriptor);
        let rpc = KasinaRpc::new(Arc::clone(&state), "test-token");
        let response = Kasina::subscribe_samples(
            &rpc,
            authenticated_request(
                SubscribeRequest {
                    client: Some(client_hello("test", "0")),
                    streams: vec![kasina_protocol::v1::StreamKind::RespirationForce as i32],
                },
                "test-token",
            )
            .unwrap(),
        )
        .await
        .unwrap();
        let mut output = response.into_inner();

        let events = stream! {
            for value in 0..(SAMPLE_BROADCAST_CAPACITY + 1_024) {
                yield DriverEvent::Measurement {
                    stream: StreamKind::RespirationForce,
                    source_id: "burst".to_owned(),
                    device_time_ns: None,
                    value: value as f64,
                    quality_flags: 0,
                };
            }
        };
        tokio::pin!(events);
        while let Some(event) = events.next().await {
            let sequence = {
                let mut sequences = state.sequences.lock();
                let next = sequences.entry(StreamKind::RespirationForce).or_insert(0);
                *next += 1;
                *next
            };
            let sample = Sample::new(
                StreamKind::RespirationForce,
                "burst",
                sequence,
                sequence,
                sequence,
                match event {
                    DriverEvent::Measurement { value, .. } => value,
                    DriverEvent::Status { .. } => unreachable!(),
                },
            );
            state.buffers.write().push(sample.clone()).unwrap();
            let _ = state.sample_sender.send(sample);
        }

        let batch = tokio::time::timeout(Duration::from_secs(1), output.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(batch.gaps.iter().any(|gap| gap.dropped_samples > 0));
        assert!(state.transport_lagged_samples.load(Ordering::Relaxed) > 0);
    }

    #[test]
    fn service_tracks_multiple_device_supervisors() {
        let state = ServiceState::new_multi(
            Duration::from_secs(30),
            [
                DeviceDescriptor {
                    id: "polar:auto".to_owned(),
                    name: "Polar H10".to_owned(),
                    kind: DeviceKind::Polar,
                },
                DeviceDescriptor {
                    id: "godirect:auto".to_owned(),
                    name: "Go Direct Respiration Belt".to_owned(),
                    kind: DeviceKind::GoDirect,
                },
            ],
        );
        let status = state.status_snapshot();
        assert_eq!(status.devices.len(), 2);
        assert_eq!(
            status
                .devices
                .iter()
                .filter_map(|device| device.device.as_ref())
                .map(|device| device.id.as_str())
                .collect::<Vec<_>>(),
            ["godirect:auto", "polar:auto"]
        );
    }

    #[tokio::test]
    async fn failed_driver_is_reported_as_disconnected() {
        let descriptor = FailingDriver.descriptor();
        let state = ServiceState::new(Duration::from_secs(30), descriptor);
        let result = Arc::clone(&state)
            .run_driver(Box::new(FailingDriver), CancellationToken::new())
            .await;
        assert!(result.is_err());
        let status = state.status_snapshot();
        assert_eq!(status.devices.len(), 1);
        assert_eq!(
            status.devices[0].state,
            ConnectionState::Disconnected as i32
        );
        assert!(
            status.devices[0]
                .detail
                .contains("intentional driver failure")
        );
    }

    #[tokio::test]
    async fn soak_diagnostics_are_append_only_json_lines() {
        let temporary = tempfile::tempdir().unwrap();
        let output = temporary.path().join("health.jsonl");
        let state = ServiceState::new(
            Duration::from_secs(30),
            SimulatedDriver::default().descriptor(),
        );
        let cancellation = CancellationToken::new();
        let writer = tokio::spawn(write_diagnostics_jsonl(
            Arc::clone(&state),
            output.clone(),
            Duration::from_millis(5),
            cancellation.clone(),
        ));

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Ok(contents) = fs::read_to_string(&output)
                    && contents.bytes().filter(|byte| *byte == b'\n').count() >= 2
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        cancellation.cancel();
        writer.await.unwrap().unwrap();

        // Read the final snapshot after the writer has flushed and closed;
        // an earlier read can end partway through a concurrently appended line.
        let contents = fs::read_to_string(&output).unwrap();
        let records = contents
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert!(records.len() >= 2);
        assert_eq!(records[0]["schema_version"], 1);
        assert_eq!(
            records[0]["service_instance_id"],
            records[1]["service_instance_id"]
        );
        assert_eq!(records[0]["devices"][0]["id"], "simulated:biofeedback");
    }
}
