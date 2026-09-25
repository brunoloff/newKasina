//! Crash-tolerant, analysis-friendly raw session recording.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead as _, BufReader, BufWriter, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use kasina_devices::{DeviceDescriptor, DeviceKind};
use kasina_domain::{Sample, StreamKind};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use uuid::Uuid;

const RECORDING_SCHEMA_VERSION: u32 = 1;
const COMMAND_CAPACITY: usize = 16_384;
const SYNC_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SessionState {
    Unavailable,
    Idle,
    Recording,
    Completed,
    Interrupted,
    Error,
}

#[derive(Debug, Clone)]
pub(crate) struct RecordingSnapshot {
    pub state: SessionState,
    pub session_id: String,
    pub label: String,
    pub directory: String,
    pub started_wall_time_unix_ns: u64,
    pub stopped_wall_time_unix_ns: Option<u64>,
    pub sample_count: u64,
    pub dropped_samples: u64,
    pub detail: String,
}

impl RecordingSnapshot {
    fn idle(detail: impl Into<String>) -> Self {
        Self {
            state: SessionState::Idle,
            session_id: String::new(),
            label: String::new(),
            directory: String::new(),
            started_wall_time_unix_ns: 0,
            stopped_wall_time_unix_ns: None,
            sample_count: 0,
            dropped_samples: 0,
            detail: detail.into(),
        }
    }

    fn unavailable() -> Self {
        Self {
            state: SessionState::Unavailable,
            detail: "recording storage is not configured for this service instance".to_owned(),
            ..Self::idle(String::new())
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionDevice {
    id: String,
    name: String,
    kind: String,
}

impl From<&DeviceDescriptor> for SessionDevice {
    fn from(device: &DeviceDescriptor) -> Self {
        Self {
            id: device.id.clone(),
            name: device.name.clone(),
            kind: match device.kind {
                DeviceKind::Polar => "polar",
                DeviceKind::GoDirect => "go_direct",
                DeviceKind::ThoughtStream => "thoughtstream",
                DeviceKind::Simulated => "simulated",
            }
            .to_owned(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct StreamSummary {
    sample_count: u64,
    first_sequence: u64,
    last_sequence: u64,
    first_wall_time_unix_ns: u64,
    last_wall_time_unix_ns: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionMetadata {
    schema_version: u32,
    session_id: String,
    label: String,
    notes: String,
    state: SessionState,
    service_instance_id: String,
    service_version: String,
    started_wall_time_unix_ns: u64,
    stopped_wall_time_unix_ns: Option<u64>,
    sample_count: u64,
    dropped_samples: u64,
    streams: BTreeMap<String, StreamSummary>,
    devices: Vec<SessionDevice>,
    samples_file: String,
    detail: String,
}

impl SessionMetadata {
    fn snapshot(&self, directory: &Path) -> RecordingSnapshot {
        RecordingSnapshot {
            state: self.state,
            session_id: self.session_id.clone(),
            label: self.label.clone(),
            directory: directory.display().to_string(),
            started_wall_time_unix_ns: self.started_wall_time_unix_ns,
            stopped_wall_time_unix_ns: self.stopped_wall_time_unix_ns,
            sample_count: self.sample_count,
            dropped_samples: self.dropped_samples,
            detail: self.detail.clone(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredSample {
    schema_version: u32,
    #[serde(
        serialize_with = "serialize_stream",
        deserialize_with = "deserialize_stream"
    )]
    stream: StreamKind,
    source_id: String,
    sequence: u64,
    monotonic_time_ns: u64,
    wall_time_unix_ns: u64,
    device_time_ns: Option<u64>,
    value: f64,
    unit: String,
    quality_flags: u32,
}

fn serialize_stream<S>(stream: &StreamKind, serializer: S) -> std::result::Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(stream_name(*stream))
}

fn deserialize_stream<'de, D>(deserializer: D) -> std::result::Result<StreamKind, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let stream = String::deserialize(deserializer)?;
    match stream.as_str() {
        "heart_rate" | "HeartRate" => Ok(StreamKind::HeartRate),
        "rr_interval" | "RrInterval" => Ok(StreamKind::RrInterval),
        "respiration_force" | "RespirationForce" => Ok(StreamKind::RespirationForce),
        "acceleration_x" | "AccelerationX" => Ok(StreamKind::AccelerationX),
        "acceleration_y" | "AccelerationY" => Ok(StreamKind::AccelerationY),
        "acceleration_z" | "AccelerationZ" => Ok(StreamKind::AccelerationZ),
        "skin_resistance" | "SkinResistance" => Ok(StreamKind::SkinResistance),
        "thoughtstream_adc" | "ThoughtStreamAdc" => Ok(StreamKind::ThoughtStreamAdc),
        _ => Err(serde::de::Error::custom(format!(
            "unrecognized recording stream {stream:?}"
        ))),
    }
}

impl From<Sample> for StoredSample {
    fn from(sample: Sample) -> Self {
        Self {
            schema_version: RECORDING_SCHEMA_VERSION,
            stream: sample.stream,
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
}

struct ActiveSession {
    directory: PathBuf,
    metadata: SessionMetadata,
    samples: BufWriter<File>,
    last_sync: Instant,
}

impl std::fmt::Debug for ActiveSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ActiveSession")
            .field("directory", &self.directory)
            .field("metadata", &self.metadata)
            .finish_non_exhaustive()
    }
}

enum RecorderCommand {
    Start {
        label: String,
        notes: String,
        service_instance_id: String,
        devices: Vec<DeviceDescriptor>,
        response: oneshot::Sender<std::result::Result<RecordingSnapshot, String>>,
    },
    Sample(Sample),
    Stop {
        response: oneshot::Sender<std::result::Result<RecordingSnapshot, String>>,
    },
    List {
        response: oneshot::Sender<std::result::Result<Vec<RecordingSnapshot>, String>>,
    },
    Shutdown,
}

/// Background recorder owned by the persistent measurement service.
pub(crate) struct RecordingManager {
    root: Option<PathBuf>,
    sender: Option<SyncSender<RecorderCommand>>,
    status: Arc<RwLock<RecordingSnapshot>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for RecordingManager {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RecordingManager")
            .field("root", &self.root)
            .field("status", &self.status.read())
            .finish_non_exhaustive()
    }
}

impl RecordingManager {
    pub fn unavailable() -> Self {
        Self {
            root: None,
            sender: None,
            status: Arc::new(RwLock::new(RecordingSnapshot::unavailable())),
            thread: Mutex::new(None),
        }
    }

    pub fn new(root: PathBuf) -> Result<Self> {
        create_private_directory(&root)?;
        recover_interrupted_sessions(&root)?;
        let (sender, receiver) = mpsc::sync_channel(COMMAND_CAPACITY);
        let status = Arc::new(RwLock::new(RecordingSnapshot::idle(format!(
            "recordings stored under {}",
            root.display()
        ))));
        let worker_status = Arc::clone(&status);
        let worker_root = root.clone();
        let thread = thread::Builder::new()
            .name("kasina-session-recorder".to_owned())
            .spawn(move || recorder_loop(&worker_root, &worker_status, &receiver))
            .context("spawn session recorder")?;
        Ok(Self {
            root: Some(root),
            sender: Some(sender),
            status,
            thread: Mutex::new(Some(thread)),
        })
    }

    pub fn snapshot(&self) -> RecordingSnapshot {
        self.status.read().clone()
    }

    pub fn record(&self, sample: Sample) {
        if self.status.read().state != SessionState::Recording {
            return;
        }
        let Some(sender) = &self.sender else {
            return;
        };
        match sender.try_send(RecorderCommand::Sample(sample)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                let mut status = self.status.write();
                status.dropped_samples = status.dropped_samples.saturating_add(1);
                status.detail = "recording queue saturated; samples were dropped".to_owned();
            }
            Err(TrySendError::Disconnected(_)) => {
                let mut status = self.status.write();
                status.state = SessionState::Error;
                status.detail = "recording writer stopped unexpectedly".to_owned();
            }
        }
    }

    pub async fn start(
        &self,
        label: String,
        notes: String,
        service_instance_id: String,
        devices: Vec<DeviceDescriptor>,
    ) -> std::result::Result<RecordingSnapshot, String> {
        let Some(sender) = &self.sender else {
            return Err(self.snapshot().detail);
        };
        let (response, receiver) = oneshot::channel();
        send_control(
            sender.clone(),
            RecorderCommand::Start {
                label,
                notes,
                service_instance_id,
                devices,
                response,
            },
        )
        .await?;
        receiver
            .await
            .map_err(|_| "recording writer discarded start response".to_owned())?
    }

    pub async fn stop(&self) -> std::result::Result<RecordingSnapshot, String> {
        let Some(sender) = &self.sender else {
            return Err(self.snapshot().detail);
        };
        let (response, receiver) = oneshot::channel();
        send_control(sender.clone(), RecorderCommand::Stop { response }).await?;
        receiver
            .await
            .map_err(|_| "recording writer discarded stop response".to_owned())?
    }

    pub async fn list(&self) -> std::result::Result<Vec<RecordingSnapshot>, String> {
        let Some(sender) = &self.sender else {
            return Err(self.snapshot().detail);
        };
        let (response, receiver) = oneshot::channel();
        send_control(sender.clone(), RecorderCommand::List { response }).await?;
        receiver
            .await
            .map_err(|_| "recording writer discarded list response".to_owned())?
    }
}

impl Drop for RecordingManager {
    fn drop(&mut self) {
        if let Some(sender) = self.sender.take() {
            let _ignored = sender.send(RecorderCommand::Shutdown);
        }
        if let Some(thread) = self.thread.lock().take()
            && thread.join().is_err()
        {
            tracing::error!("session recorder thread panicked");
        }
    }
}

async fn send_control(
    sender: SyncSender<RecorderCommand>,
    command: RecorderCommand,
) -> std::result::Result<(), String> {
    tokio::task::spawn_blocking(move || sender.send(command))
        .await
        .map_err(|_| "recording command task panicked".to_owned())?
        .map_err(|_| "recording writer stopped".to_owned())
}

fn recorder_loop(
    root: &Path,
    status: &RwLock<RecordingSnapshot>,
    receiver: &mpsc::Receiver<RecorderCommand>,
) {
    let mut active: Option<ActiveSession> = None;
    while let Ok(command) = receiver.recv() {
        match command {
            RecorderCommand::Start {
                label,
                notes,
                service_instance_id,
                devices,
                response,
            } => {
                let result = if active.is_some() {
                    Err("a recording is already active".to_owned())
                } else {
                    start_session(root, label, notes, service_instance_id, &devices)
                        .map(|session| {
                            let snapshot = session.metadata.snapshot(&session.directory);
                            *status.write() = snapshot.clone();
                            active = Some(session);
                            snapshot
                        })
                        .map_err(|error| format!("start recording: {error:#}"))
                };
                if let Err(error) = &result {
                    let mut snapshot = status.write();
                    snapshot.detail.clone_from(error);
                    if snapshot.state != SessionState::Recording {
                        snapshot.state = SessionState::Error;
                    }
                }
                let _ignored = response.send(result);
            }
            RecorderCommand::Sample(sample) => {
                if let Some(session) = &mut active
                    && let Err(error) = append_sample(session, sample, status)
                {
                    let detail = format!("record sample: {error:#}");
                    session.metadata.state = SessionState::Error;
                    session.metadata.detail.clone_from(&detail);
                    let _ignored = finish_session(session, SessionState::Error, &detail, status);
                    active = None;
                }
            }
            RecorderCommand::Stop { response } => {
                let result = if let Some(mut session) = active.take() {
                    finish_session(
                        &mut session,
                        SessionState::Completed,
                        "recording stopped cleanly",
                        status,
                    )
                    .map_err(|error| format!("stop recording: {error:#}"))
                } else {
                    Err("no recording is active".to_owned())
                };
                let _ignored = response.send(result);
            }
            RecorderCommand::List { response } => {
                let result = scan_sessions(root)
                    .map(|mut sessions| {
                        if active.is_some() {
                            let current = status.read().clone();
                            if let Some(stored) = sessions
                                .iter_mut()
                                .find(|stored| stored.session_id == current.session_id)
                            {
                                *stored = current;
                            }
                        }
                        sessions
                    })
                    .map_err(|error| format!("list recordings: {error:#}"));
                let _ignored = response.send(result);
            }
            RecorderCommand::Shutdown => {
                if let Some(mut session) = active.take() {
                    let _ignored = finish_session(
                        &mut session,
                        SessionState::Completed,
                        "recording stopped during clean service shutdown",
                        status,
                    );
                }
                break;
            }
        }
    }
}

fn start_session(
    root: &Path,
    label: String,
    notes: String,
    service_instance_id: String,
    devices: &[DeviceDescriptor],
) -> Result<ActiveSession> {
    let label = clean_label(&label)?;
    let notes: String = notes.trim().chars().take(4_000).collect();
    let started = crate::unix_time_ns();
    let session_id = Uuid::new_v4().to_string();
    let directory = session_directory(root, started, &label, &session_id);
    create_private_directory(&directory)?;
    let samples_path = directory.join("samples.jsonl");
    let samples = BufWriter::new(open_private_new(&samples_path)?);
    let metadata = SessionMetadata {
        schema_version: RECORDING_SCHEMA_VERSION,
        session_id,
        label,
        notes,
        state: SessionState::Recording,
        service_instance_id,
        service_version: env!("CARGO_PKG_VERSION").to_owned(),
        started_wall_time_unix_ns: started,
        stopped_wall_time_unix_ns: None,
        sample_count: 0,
        dropped_samples: 0,
        streams: BTreeMap::new(),
        devices: devices.iter().map(SessionDevice::from).collect(),
        samples_file: "samples.jsonl".to_owned(),
        detail: "recording raw service samples".to_owned(),
    };
    write_metadata(&directory, &metadata)?;
    Ok(ActiveSession {
        directory,
        metadata,
        samples,
        last_sync: Instant::now(),
    })
}

fn append_sample(
    session: &mut ActiveSession,
    sample: Sample,
    status: &RwLock<RecordingSnapshot>,
) -> Result<()> {
    let stream_name = stream_name(sample.stream).to_owned();
    let summary = session.metadata.streams.entry(stream_name).or_default();
    if summary.sample_count == 0 {
        summary.first_sequence = sample.sequence;
        summary.first_wall_time_unix_ns = sample.wall_time_unix_ns;
    }
    summary.sample_count = summary.sample_count.saturating_add(1);
    summary.last_sequence = sample.sequence;
    summary.last_wall_time_unix_ns = sample.wall_time_unix_ns;
    session.metadata.sample_count = session.metadata.sample_count.saturating_add(1);

    serde_json::to_writer(&mut session.samples, &StoredSample::from(sample))
        .context("serialize raw sample")?;
    session.samples.write_all(b"\n")?;
    session.samples.flush()?;
    if session.last_sync.elapsed() >= SYNC_INTERVAL {
        session.samples.get_ref().sync_data()?;
        session.last_sync = Instant::now();
    }
    let mut current = status.write();
    session.metadata.dropped_samples = current.dropped_samples;
    *current = session.metadata.snapshot(&session.directory);
    Ok(())
}

fn finish_session(
    session: &mut ActiveSession,
    state: SessionState,
    detail: &str,
    status: &RwLock<RecordingSnapshot>,
) -> Result<RecordingSnapshot> {
    session.samples.flush()?;
    session.samples.get_ref().sync_all()?;
    session.metadata.state = state;
    session.metadata.stopped_wall_time_unix_ns = Some(crate::unix_time_ns());
    session.metadata.dropped_samples = status.read().dropped_samples;
    session.metadata.detail = detail.to_owned();
    write_metadata(&session.directory, &session.metadata)?;
    let snapshot = session.metadata.snapshot(&session.directory);
    *status.write() = snapshot.clone();
    Ok(snapshot)
}

fn scan_sessions(root: &Path) -> Result<Vec<RecordingSnapshot>> {
    let mut metadata_paths = Vec::new();
    collect_metadata_paths(root, &mut metadata_paths)?;
    let mut sessions = Vec::with_capacity(metadata_paths.len());
    for path in metadata_paths {
        let metadata: SessionMetadata = match File::open(&path)
            .with_context(|| format!("open {}", path.display()))
            .and_then(|file| {
                serde_json::from_reader(file).with_context(|| format!("parse {}", path.display()))
            }) {
            Ok(metadata) => metadata,
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "skip unreadable session metadata");
                continue;
            }
        };
        let directory = path.parent().context("metadata path has no parent")?;
        sessions.push(metadata.snapshot(directory));
    }
    sessions.sort_by_key(|session| std::cmp::Reverse(session.started_wall_time_unix_ns));
    Ok(sessions)
}

fn recover_interrupted_sessions(root: &Path) -> Result<()> {
    let mut metadata_paths = Vec::new();
    collect_metadata_paths(root, &mut metadata_paths)?;
    for path in metadata_paths {
        let mut metadata: SessionMetadata = match serde_json::from_reader(File::open(&path)?) {
            Ok(metadata) => metadata,
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "skip unreadable session metadata");
                continue;
            }
        };
        if metadata.state != SessionState::Recording {
            continue;
        }
        let directory = path.parent().context("metadata path has no parent")?;
        let samples_path = directory.join(&metadata.samples_file);
        let recovered = summarize_samples(&samples_path)?;
        metadata.sample_count = recovered.values().map(|summary| summary.sample_count).sum();
        metadata.stopped_wall_time_unix_ns = recovered
            .values()
            .map(|summary| summary.last_wall_time_unix_ns)
            .max()
            .or(Some(metadata.started_wall_time_unix_ns));
        metadata.streams = recovered;
        metadata.state = SessionState::Interrupted;
        metadata.detail =
            "service ended before StopRecording; complete JSONL samples recovered".to_owned();
        write_metadata(directory, &metadata)?;
    }
    Ok(())
}

fn summarize_samples(path: &Path) -> Result<BTreeMap<String, StreamSummary>> {
    let mut streams = BTreeMap::<String, StreamSummary>::new();
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(streams),
        Err(error) => return Err(error).with_context(|| format!("open {}", path.display())),
    };
    for line in BufReader::new(file).lines() {
        let line = line?;
        let sample: StoredSample = match serde_json::from_str(&line) {
            Ok(sample) => sample,
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "ignore truncated recording tail");
                break;
            }
        };
        let summary = streams
            .entry(stream_name(sample.stream).to_owned())
            .or_default();
        if summary.sample_count == 0 {
            summary.first_sequence = sample.sequence;
            summary.first_wall_time_unix_ns = sample.wall_time_unix_ns;
        }
        summary.sample_count = summary.sample_count.saturating_add(1);
        summary.last_sequence = sample.sequence;
        summary.last_wall_time_unix_ns = sample.wall_time_unix_ns;
    }
    Ok(streams)
}

fn collect_metadata_paths(directory: &Path, output: &mut Vec<PathBuf>) -> Result<()> {
    if !directory.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            collect_metadata_paths(&path, output)?;
        } else if entry.file_name() == "metadata.json" {
            output.push(path);
        }
    }
    Ok(())
}

fn write_metadata(directory: &Path, metadata: &SessionMetadata) -> Result<()> {
    let path = directory.join("metadata.json");
    let temporary = directory.join("metadata.json.tmp");
    let mut file = open_private_replace(&temporary)?;
    serde_json::to_writer_pretty(&mut file, metadata)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    // Windows ReplaceFileW needs to open the replacement file without our
    // write handle still holding it. The contents are durable before closing.
    drop(file);
    replace_metadata_file(&temporary, &path)?;
    #[cfg(unix)]
    if let Err(error) = File::open(directory).and_then(|directory| directory.sync_all()) {
        tracing::warn!(%error, path = %directory.display(), "could not sync recording directory entry");
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_metadata_file(temporary: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(temporary, destination)
}

#[cfg(windows)]
fn replace_metadata_file(temporary: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;

    if !destination.exists() {
        return fs::rename(temporary, destination);
    }
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let temporary = temporary
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // SAFETY: both paths are valid, NUL-terminated UTF-16 buffers that remain alive for
    // the call; the optional backup and reserved pointers are explicitly null.
    let replaced = unsafe {
        ReplaceFileW(
            destination.as_ptr(),
            temporary.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if replaced == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn clean_label(label: &str) -> Result<String> {
    let label: String = label.trim().chars().take(100).collect();
    if label.is_empty() {
        bail!("recording label cannot be empty");
    }
    Ok(label)
}

fn session_directory(root: &Path, wall_time_ns: u64, label: &str, session_id: &str) -> PathBuf {
    let timestamp = utc_timestamp(wall_time_ns / 1_000_000_000);
    let slug = label_slug(label);
    root.join(&timestamp[0..4])
        .join(&timestamp[4..6])
        .join(&timestamp[6..8])
        .join(format!(
            "{}_{}_{}",
            timestamp,
            slug,
            &session_id[..8.min(session_id.len())]
        ))
}

fn label_slug(label: &str) -> String {
    let mut slug = String::new();
    let mut separator = false;
    for character in label.chars() {
        if character.is_ascii_alphanumeric() {
            if separator && !slug.is_empty() {
                slug.push('-');
            }
            slug.push(character.to_ascii_lowercase());
            separator = false;
        } else {
            separator = true;
        }
        if slug.len() >= 40 {
            break;
        }
    }
    if slug.is_empty() {
        "session".to_owned()
    } else {
        slug
    }
}

fn utc_timestamp(seconds: u64) -> String {
    let days = (seconds / 86_400).min(i64::MAX as u64) as i64;
    let day_seconds = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = day_seconds / 3_600;
    let minute = day_seconds % 3_600 / 60;
    let second = day_seconds % 60;
    format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z")
}

// Howard Hinnant's public-domain inverse civil-calendar algorithm.
fn civil_from_days(days_since_epoch: i64) -> (i64, u64, u64) {
    let shifted = days_since_epoch + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month as u64, day as u64)
}

fn stream_name(stream: StreamKind) -> &'static str {
    match stream {
        StreamKind::HeartRate => "heart_rate",
        StreamKind::RrInterval => "rr_interval",
        StreamKind::RespirationForce => "respiration_force",
        StreamKind::AccelerationX => "acceleration_x",
        StreamKind::AccelerationY => "acceleration_y",
        StreamKind::AccelerationZ => "acceleration_z",
        StreamKind::SkinResistance => "skin_resistance",
        StreamKind::ThoughtStreamAdc => "thoughtstream_adc",
    }
}

fn create_private_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn open_private_new(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    private_mode(&mut options);
    options
        .open(path)
        .with_context(|| format!("create {}", path.display()))
}

fn open_private_replace(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    private_mode(&mut options);
    options
        .open(path)
        .with_context(|| format!("open {}", path.display()))
}

#[cfg(unix)]
fn private_mode(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt as _;
    options.mode(0o600);
}

#[cfg(not(unix))]
fn private_mode(_options: &mut OpenOptions) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_and_session_paths_are_stable_and_safe() {
        assert_eq!(utc_timestamp(0), "19700101T000000Z");
        assert_eq!(utc_timestamp(1_723_510_800), "20240813T010000Z");
        let path = session_directory(
            Path::new("sessions"),
            1_723_510_800_000_000_000,
            "Morning breath / calm",
            "12345678-abcd",
        );
        assert_eq!(
            path,
            Path::new("sessions/2024/08/13/20240813T010000Z_morning-breath-calm_12345678")
        );
    }

    #[tokio::test]
    async fn recording_round_trip_preserves_raw_samples_and_metadata() {
        let temporary = tempfile::tempdir().unwrap();
        let manager = RecordingManager::new(temporary.path().join("sessions")).unwrap();
        let started = manager
            .start(
                "Test session".to_owned(),
                "consent-safe synthetic fixture".to_owned(),
                "service-instance".to_owned(),
                vec![DeviceDescriptor {
                    id: "simulated:test".to_owned(),
                    name: "Synthetic source".to_owned(),
                    kind: DeviceKind::Simulated,
                }],
            )
            .await
            .unwrap();
        for sequence in 1..=3 {
            let mut sample = Sample::new(
                StreamKind::RespirationForce,
                "simulated:test",
                sequence,
                sequence * 100,
                1_000 + sequence * 100,
                sequence as f64 * 1.25,
            );
            sample.quality_flags = kasina_domain::quality::SIMULATED;
            manager.record(sample);
        }
        let stopped = manager.stop().await.unwrap();

        assert_eq!(started.state, SessionState::Recording);
        assert_eq!(stopped.state, SessionState::Completed);
        assert_eq!(stopped.sample_count, 3);
        assert_eq!(stopped.dropped_samples, 0);
        let metadata: SessionMetadata = serde_json::from_reader(
            File::open(Path::new(&stopped.directory).join("metadata.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(metadata.streams["respiration_force"].last_sequence, 3);
        let lines =
            fs::read_to_string(Path::new(&stopped.directory).join("samples.jsonl")).unwrap();
        let samples = lines
            .lines()
            .map(|line| serde_json::from_str::<StoredSample>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(samples.len(), 3);
        assert_eq!(samples[2].value, 3.75);
        assert_eq!(samples[2].quality_flags, kasina_domain::quality::SIMULATED);
        let listed = manager.list().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].session_id, stopped.session_id);
    }

    #[tokio::test]
    async fn clean_manager_shutdown_finalizes_an_active_recording() {
        let temporary = tempfile::tempdir().unwrap();
        let manager = RecordingManager::new(temporary.path().join("sessions")).unwrap();
        let active = manager
            .start(
                "Shutdown test".to_owned(),
                String::new(),
                "service-instance".to_owned(),
                Vec::new(),
            )
            .await
            .unwrap();
        manager.record(Sample::new(
            StreamKind::HeartRate,
            "simulated:test",
            1,
            100,
            1_000,
            64.0,
        ));
        drop(manager);

        let metadata: SessionMetadata = serde_json::from_reader(
            File::open(Path::new(&active.directory).join("metadata.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(metadata.state, SessionState::Completed);
        assert_eq!(metadata.sample_count, 1);
        assert_eq!(
            metadata.detail,
            "recording stopped during clean service shutdown"
        );
    }

    #[test]
    fn interrupted_recording_recovers_complete_lines_and_ignores_truncated_tail() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("sessions");
        let directory = root.join("2026/08/13/test");
        create_private_directory(&directory).unwrap();
        let metadata = SessionMetadata {
            schema_version: 1,
            session_id: "interrupted".to_owned(),
            label: "Interrupted test".to_owned(),
            notes: String::new(),
            state: SessionState::Recording,
            service_instance_id: "old-service".to_owned(),
            service_version: "0".to_owned(),
            started_wall_time_unix_ns: 100,
            stopped_wall_time_unix_ns: None,
            sample_count: 0,
            dropped_samples: 0,
            streams: BTreeMap::new(),
            devices: Vec::new(),
            samples_file: "samples.jsonl".to_owned(),
            detail: String::new(),
        };
        write_metadata(&directory, &metadata).unwrap();
        let sample = StoredSample {
            schema_version: 1,
            stream: StreamKind::HeartRate,
            source_id: "test".to_owned(),
            sequence: 4,
            monotonic_time_ns: 50,
            wall_time_unix_ns: 150,
            device_time_ns: None,
            value: 64.0,
            unit: "bpm".to_owned(),
            quality_flags: 0,
        };
        let mut file = open_private_new(&directory.join("samples.jsonl")).unwrap();
        serde_json::to_writer(&mut file, &sample).unwrap();
        file.write_all(b"\n{\"truncated\":").unwrap();
        file.sync_all().unwrap();

        recover_interrupted_sessions(&root).unwrap();
        let recovered: SessionMetadata =
            serde_json::from_reader(File::open(directory.join("metadata.json")).unwrap()).unwrap();
        assert_eq!(recovered.state, SessionState::Interrupted);
        assert_eq!(recovered.sample_count, 1);
        assert_eq!(recovered.streams["heart_rate"].last_sequence, 4);
    }
}
