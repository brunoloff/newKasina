//! Persistent acquisition state and authenticated streaming RPC implementation.

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
use kasina_devices::{DeviceDescriptor, DeviceKind, DriverEvent, SensorDriver};
use kasina_domain::{Sample, ServiceBuffers, StreamKind};
use kasina_protocol::v1::kasina_server::{Kasina, KasinaServer};
use kasina_protocol::v1::{
    ClientHello, ConnectionState, DeviceCommand, DeviceInfo, DeviceList, DeviceStatus,
    PreferredDeviceCommand, SampleBatch, SamplesSinceRequest, ServiceInfo, StatusSnapshot,
    StreamDiagnostics, StreamGap, SubscribeRequest,
};
use kasina_protocol::{AUTH_HEADER, PROTOCOL_MAJOR, PROTOCOL_MINOR};
use parking_lot::{Mutex, RwLock};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::TcpListenerStream;
use tokio_util::sync::CancellationToken;
use tonic::transport::Server;
use tonic::{Request, Response, Status};
use uuid::Uuid;

/// Default local service port, retained from the Python implementation.
pub const DEFAULT_PORT: u16 = 18_861;
/// Short transport batching interval.
pub const BATCH_INTERVAL: Duration = Duration::from_millis(20);
const SAMPLE_BROADCAST_CAPACITY: usize = 4_096;

/// Paths used by the per-user service.
#[derive(Debug, Clone)]
pub struct ServicePaths {
    /// Random authentication token file.
    pub token: PathBuf,
    /// Single-instance lock file.
    pub lock: PathBuf,
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
            restrict_token_permissions(path)?;
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
fn restrict_token_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_token_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[derive(Debug, Clone)]
struct DeviceRuntime {
    descriptor: DeviceDescriptor,
    active_source_id: String,
    connected: bool,
    finished: bool,
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
                        connected: false,
                        finished: false,
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
                    let _ = self.sample_sender.send(sample);
                }
                DriverEvent::Status { source_id, detail } => {
                    if let Some(device) = self.devices.write().get_mut(&runtime_id) {
                        device.active_source_id = source_id;
                        device.connected = detail == "connected";
                        if detail.starts_with("reconnecting") {
                            device.reconnect_attempts = device.reconnect_attempts.saturating_add(1);
                        }
                        device.detail = detail;
                    }
                }
            }
        }

        driver_cancel.cancel();
        driver_task.await.context("driver task panicked")??;
        if let Some(device) = self.devices.write().get_mut(&runtime_id) {
            device.connected = false;
            device.finished = true;
            device.detail = if cancellation.is_cancelled() {
                "stopped".to_owned()
            } else {
                "driver finished".to_owned()
            };
        }
        Ok(())
    }

    fn status_snapshot(&self) -> StatusSnapshot {
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
            state: if runtime.connected {
                ConnectionState::Connected as i32
            } else if runtime.finished {
                ConnectionState::Disconnected as i32
            } else {
                ConnectionState::Connecting as i32
            },
            detail: runtime.detail.clone(),
            reconnect_attempts: runtime.reconnect_attempts,
            last_sample_age_millis: last_sample_time
                .map_or(0, |time| now_ns.saturating_sub(time) / 1_000_000),
        }
    }
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
        Proto::Unspecified => Err(Status::invalid_argument("stream is unspecified")),
    }
}

fn proto_device_kind(kind: DeviceKind) -> kasina_protocol::v1::DeviceKind {
    use kasina_protocol::v1::DeviceKind as Proto;
    match kind {
        DeviceKind::Polar => Proto::Polar,
        DeviceKind::GoDirect => Proto::GoDirect,
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
}
