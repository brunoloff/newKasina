#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod service_host;
mod service_panel;
mod settings;
mod thoughtstream;

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, anyhow, bail};
use clap::{Parser, ValueEnum};
use kasina_devices::simulated_values;
use kasina_domain::quality;
use kasina_protocol::v1::kasina_client::KasinaClient;
use kasina_protocol::v1::{
    DeviceCommand, RecordingState, RecordingStatus, Sample, SampleBatch, SamplesSinceRequest,
    ServiceInfo, StartRecordingRequest, StatusSnapshot, StopRecordingRequest, StreamCursor,
    StreamKind, SubscribeRequest, ThoughtStreamPortCommand, ThoughtStreamPorts,
};
use kasina_protocol::{AUTH_HEADER, client_hello};
use kasina_render::{
    BiofeedbackRenderer, FrameStats, KasinaAnimationInput, KasinaVisual, LuminousMandala,
    OrganicKaleidoscope, PaperDisk,
};
use serde::Serialize;
use settings::{AppSettings, KasinaVisualPreset, SettingsWriter};
use tokio_util::sync::CancellationToken;
use tonic::Request;
use tonic::metadata::MetadataValue;
use tonic::transport::Endpoint;
use tracing::{debug, warn};
use tracing_subscriber::EnvFilter;

const HISTORY_CAPACITY_PER_STREAM: usize = 6_000;
const UI_EVENT_CAPACITY: usize = 256;
const ANIMATION_INTERVAL: Duration = Duration::from_millis(8);
const SIMULATION_SAMPLE_INTERVAL: Duration = Duration::from_millis(100);
const SIMULATION_CATCH_UP_SAMPLES: u64 = 600;
const BREATH_ENVELOPE_RELAXATION: f64 = 0.0025;
const MINIMUM_FORCE_SPAN: f64 = 0.01;
#[derive(Debug, Parser)]
#[command(about = "newKasina biofeedback desktop client")]
struct Args {
    /// Loopback acquisition service endpoint.
    #[arg(long, default_value = "http://127.0.0.1:18861")]
    endpoint: String,
    /// Override the standard service authentication-token path.
    #[arg(long)]
    token_path: Option<PathBuf>,
    /// Service launch policy. Auto keeps Linux's separate tray workflow.
    #[arg(long, value_enum, default_value_t = ServiceMode::Auto)]
    service_mode: ServiceMode,
    /// Run an isolated simulated GUI/service check, then exit. Never opens hardware.
    #[arg(long, value_parser = benchmark_duration_seconds, requires = "smoke_test_output")]
    smoke_test_seconds: Option<f64>,
    /// JSON result of an isolated GUI/service check.
    #[arg(long, requires = "smoke_test_seconds")]
    smoke_test_output: Option<PathBuf>,
    /// Run the visualizer for this many measured seconds, write JSON, and exit.
    #[arg(long, value_parser = benchmark_duration_seconds)]
    render_benchmark_seconds: Option<f64>,
    /// Warm-up time excluded from render benchmark statistics.
    #[arg(long, default_value_t = 2.0, value_parser = benchmark_warmup_seconds)]
    benchmark_warmup_seconds: f64,
    /// JSON destination for a render benchmark (default: render-benchmark.json).
    #[arg(long)]
    benchmark_output: Option<PathBuf>,
    /// Particle instances drawn by the stress visualizer.
    #[arg(long, default_value_t = 8_000, value_parser = clap::value_parser!(u32).range(1..=1_000_000))]
    stress_instances: u32,
    /// Externally confirmed display refresh rate for evaluating the frame-pacing budget.
    #[arg(long, value_parser = refresh_rate_hz)]
    display_refresh_hz: Option<f64>,
    /// Required application update rate; defaults to the declared display refresh rate.
    #[arg(long, value_parser = refresh_rate_hz)]
    performance_target_hz: Option<f64>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ServiceMode {
    Auto,
    External,
    Embedded,
}

fn finite_number(value: &str) -> std::result::Result<f64, String> {
    let parsed = value
        .parse::<f64>()
        .map_err(|error| format!("invalid number: {error}"))?;
    if parsed.is_finite() {
        Ok(parsed)
    } else {
        Err("value must be finite".to_owned())
    }
}

fn benchmark_duration_seconds(value: &str) -> std::result::Result<f64, String> {
    let parsed = finite_number(value)?;
    if parsed > 0.0 && parsed <= 300.0 {
        Ok(parsed)
    } else {
        Err("benchmark duration must be greater than zero and at most 300 seconds".to_owned())
    }
}

fn benchmark_warmup_seconds(value: &str) -> std::result::Result<f64, String> {
    let parsed = finite_number(value)?;
    if (0.0..=60.0).contains(&parsed) {
        Ok(parsed)
    } else {
        Err("benchmark warm-up must be between zero and 60 seconds".to_owned())
    }
}

fn refresh_rate_hz(value: &str) -> std::result::Result<f64, String> {
    let parsed = finite_number(value)?;
    if (1.0..=1_000.0).contains(&parsed) {
        Ok(parsed)
    } else {
        Err("display refresh must be between 1 and 1000 Hz".to_owned())
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let args = Args::parse();
    let smoke_output = args.smoke_test_output.clone();
    let smoke_persistence = args
        .smoke_test_seconds
        .map(|_| tempfile::tempdir())
        .transpose()?;
    let surface_health = Arc::new(SurfaceHealth::default());
    let wgpu_options = eframe::WgpuConfiguration {
        on_surface_status: surface_status_handler(Arc::clone(&surface_health)),
        ..Default::default()
    };
    eframe::run_native(
        "newKasina",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([1_180.0, 780.0])
                .with_min_inner_size([760.0, 500.0]),
            wgpu_options,
            persist_window: smoke_persistence.is_none(),
            persistence_path: smoke_persistence
                .as_ref()
                .map(|directory| directory.path().join("window.ron")),
            ..Default::default()
        },
        Box::new(move |creation_context| {
            Ok(Box::new(KasinaApp::new(
                creation_context,
                args,
                surface_health,
            )?))
        }),
    )
    .map_err(|error| anyhow!(error.to_string()))?;
    if let Some(path) = smoke_output {
        let report: serde_json::Value = serde_json::from_slice(&fs::read(path)?)?;
        if report["success"] != true {
            bail!("application smoke check failed: {}", report["detail"]);
        }
    }
    Ok(())
}

#[derive(Debug, Default)]
struct SurfaceHealth {
    outdated: AtomicU64,
    lost: AtomicU64,
    occluded: AtomicU64,
    other: AtomicU64,
}

fn surface_status_handler(
    health: Arc<SurfaceHealth>,
) -> Arc<
    dyn Fn(&eframe::wgpu::CurrentSurfaceTexture) -> eframe::egui_wgpu::SurfaceErrorAction
        + Send
        + Sync,
> {
    Arc::new(move |status| match status {
        eframe::wgpu::CurrentSurfaceTexture::Outdated => {
            health.outdated.fetch_add(1, Ordering::Relaxed);
            eframe::egui_wgpu::SurfaceErrorAction::Reconfigure
        }
        eframe::wgpu::CurrentSurfaceTexture::Lost => {
            health.lost.fetch_add(1, Ordering::Relaxed);
            eframe::egui_wgpu::SurfaceErrorAction::RecreateSurface
        }
        eframe::wgpu::CurrentSurfaceTexture::Occluded => {
            health.occluded.fetch_add(1, Ordering::Relaxed);
            eframe::egui_wgpu::SurfaceErrorAction::SkipFrame
        }
        _ => {
            health.other.fetch_add(1, Ordering::Relaxed);
            eframe::egui_wgpu::SurfaceErrorAction::SkipFrame
        }
    })
}

fn default_token_path() -> Result<PathBuf> {
    let project = directories::ProjectDirs::from("org", "newkasina", "newKasina")
        .context("operating system did not provide a user configuration directory")?;
    Ok(project.config_dir().join("service-token"))
}

fn default_settings_path() -> Result<PathBuf> {
    let project = directories::ProjectDirs::from("org", "newkasina", "newKasina")
        .context("operating system did not provide a user configuration directory")?;
    Ok(project.config_dir().join("app-settings.json"))
}

#[derive(Debug)]
enum ClientEvent {
    Connection(String),
    Connected(bool),
    ServiceInfo(ServiceInfo),
    Status(StatusSnapshot),
    Samples(SampleBatch),
    Recording(RecordingStatus),
    RecordingError(String),
    SerialPorts(ThoughtStreamPorts),
    ServiceCommandComplete(Option<String>),
}

#[derive(Debug)]
enum NetworkCommand {
    StartRecording { label: String, notes: String },
    StopRecording,
    SetDeviceEnabled { device_id: String, enabled: bool },
    ReconnectDevice { device_id: String },
    RefreshSerialPorts,
    SetThoughtStreamPort(Option<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    Service,
    Dashboard,
    Raw,
    BreathKasina,
    ThoughtStream,
    Visualizer,
    Diagnostics,
    Settings,
}

fn repaint_interval(
    view: View,
    viewport_visible: Option<bool>,
    simulation_mode: bool,
) -> Option<Duration> {
    if viewport_visible == Some(false) {
        None
    } else if matches!(view, View::BreathKasina | View::Visualizer) {
        Some(ANIMATION_INTERVAL)
    } else if simulation_mode || matches!(view, View::ThoughtStream | View::Service) {
        Some(SIMULATION_SAMPLE_INTERVAL)
    } else {
        None
    }
}

#[derive(Debug)]
struct RenderBenchmark {
    warmup: Duration,
    duration: Duration,
    measurement_start: Instant,
    measurement_end: Instant,
    output: PathBuf,
    frame_intervals: FrameStats,
    ui_cpu_times: FrameStats,
    writer: Option<JoinHandle<std::result::Result<(), String>>>,
    submitted: bool,
}

impl RenderBenchmark {
    fn new(now: Instant, warmup_seconds: f64, duration_seconds: f64, output: PathBuf) -> Self {
        let warmup = Duration::from_secs_f64(warmup_seconds);
        let duration = Duration::from_secs_f64(duration_seconds);
        let measurement_start = now + warmup;
        let sample_capacity =
            (duration_seconds.mul_add(240.0, 1_024.0).ceil() as usize).min(100_000);
        Self {
            warmup,
            duration,
            measurement_start,
            measurement_end: measurement_start + duration,
            output,
            frame_intervals: FrameStats::new(sample_capacity),
            ui_cpu_times: FrameStats::new(sample_capacity),
            writer: None,
            submitted: false,
        }
    }

    fn recording(&self, now: Instant) -> bool {
        now >= self.measurement_start && now < self.measurement_end
    }
}

#[derive(Debug, Serialize)]
struct RenderBenchmarkReport {
    schema_version: u32,
    package_version: &'static str,
    source_revision: &'static str,
    operating_system: &'static str,
    architecture: &'static str,
    adapter: String,
    backend: String,
    device_type: String,
    driver: String,
    driver_info: String,
    hardware_accelerated: bool,
    present_mode: String,
    desired_maximum_frame_latency: Option<u32>,
    logical_viewport_points: Option<[f32; 2]>,
    physical_viewport_pixels: Option<[u32; 2]>,
    native_pixels_per_point: Option<f32>,
    fullscreen: Option<bool>,
    stress_instances: u32,
    declared_display_refresh_hz: Option<f64>,
    performance_target_hz: Option<f64>,
    target_frame_interval_ms: Option<f64>,
    sample_count_sufficient: Option<bool>,
    target_met: Option<bool>,
    warmup_seconds: f64,
    measured_seconds: f64,
    measured_frames: usize,
    achieved_frames_per_second: f64,
    frame_interval_average_ms: f64,
    frame_interval_p95_ms: f64,
    frame_interval_p99_ms: f64,
    ui_cpu_average_ms: f64,
    ui_cpu_p95_ms: f64,
    ui_cpu_p99_ms: f64,
    wgpu_prepare_average_ms: f64,
    wgpu_prepare_p95_ms: f64,
    wgpu_prepare_p99_ms: f64,
    latest_upload_bytes: u64,
    surface_outdated_events: u64,
    surface_lost_events: u64,
    surface_occluded_events: u64,
    surface_other_events: u64,
    device_lost: Option<String>,
    note: &'static str,
}

fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

fn frame_target_result(
    hardware_accelerated: bool,
    target_hz: Option<f64>,
    measured_duration: Duration,
    measured_frames: usize,
    p99_ms: f64,
) -> (Option<f64>, Option<bool>, Option<bool>) {
    let Some(target_hz) = target_hz else {
        return (None, None, None);
    };
    let target_ms = 1_000.0 / target_hz;
    let enough_samples =
        measured_frames as f64 >= measured_duration.as_secs_f64() * target_hz * 0.5;
    (
        Some(target_ms),
        Some(enough_samples),
        Some(hardware_accelerated && enough_samples && p99_ms < target_ms),
    )
}

#[derive(Debug)]
struct ClientModel {
    connection: String,
    connected: bool,
    service_info: Option<ServiceInfo>,
    status: Option<StatusSnapshot>,
    samples: BTreeMap<i32, VecDeque<Sample>>,
    last_sequences: BTreeMap<i32, u64>,
    explicit_gap_samples: u64,
    inferred_gap_samples: u64,
    duplicate_samples: u64,
    recording: Option<RecordingStatus>,
}

impl Default for ClientModel {
    fn default() -> Self {
        Self {
            connection: "waiting for acquisition service".to_owned(),
            connected: false,
            service_info: None,
            status: None,
            samples: BTreeMap::new(),
            last_sequences: BTreeMap::new(),
            explicit_gap_samples: 0,
            inferred_gap_samples: 0,
            duplicate_samples: 0,
            recording: None,
        }
    }
}

impl ClientModel {
    fn apply_service_info(&mut self, info: ServiceInfo) {
        if self
            .service_info
            .as_ref()
            .is_some_and(|previous| previous.instance_id != info.instance_id)
        {
            self.samples.clear();
            self.last_sequences.clear();
            self.status = None;
            self.recording = None;
            self.explicit_gap_samples = 0;
            self.inferred_gap_samples = 0;
            self.duplicate_samples = 0;
        }
        self.service_info = Some(info);
    }
    fn apply_batch(&mut self, batch: SampleBatch) {
        self.explicit_gap_samples = self.explicit_gap_samples.saturating_add(
            batch
                .gaps
                .iter()
                .map(|gap| gap.dropped_samples)
                .sum::<u64>(),
        );
        for sample in batch.samples {
            let previous = self
                .last_sequences
                .get(&sample.stream)
                .copied()
                .unwrap_or(0);
            if sample.sequence <= previous {
                self.duplicate_samples = self.duplicate_samples.saturating_add(1);
                continue;
            }
            if previous != 0 && sample.sequence > previous.saturating_add(1) {
                self.inferred_gap_samples = self
                    .inferred_gap_samples
                    .saturating_add(sample.sequence - previous - 1);
            }
            self.last_sequences.insert(sample.stream, sample.sequence);
            let history = self.samples.entry(sample.stream).or_default();
            if history.len() == HISTORY_CAPACITY_PER_STREAM {
                history.pop_front();
            }
            history.push_back(sample);
        }
    }

    fn latest(&self, stream: StreamKind) -> Option<&Sample> {
        self.samples.get(&(stream as i32))?.back()
    }
}

#[derive(Debug)]
struct SimulationState {
    model: ClientModel,
    started: Instant,
    wall_time_epoch_ns: u64,
    next_tick: u64,
}

impl SimulationState {
    fn new(now: Instant) -> Self {
        Self {
            model: ClientModel {
                connection: "simulation mode · no sensors required".to_owned(),
                ..ClientModel::default()
            },
            started: now,
            wall_time_epoch_ns: unix_time_ns(),
            next_tick: 0,
        }
    }

    fn restart(&mut self, now: Instant) {
        *self = Self::new(now);
    }

    fn update(&mut self, now: Instant) -> Option<f64> {
        let elapsed_tick =
            now.duration_since(self.started).as_nanos() / SIMULATION_SAMPLE_INTERVAL.as_nanos();
        let elapsed_tick = elapsed_tick.min(u128::from(u64::MAX)) as u64;
        if self.next_tick > elapsed_tick {
            return None;
        }

        let first_tick = self
            .next_tick
            .max(elapsed_tick.saturating_sub(SIMULATION_CATCH_UP_SAMPLES - 1));
        let mut latest_force = None;
        for tick in first_tick..=elapsed_tick {
            let monotonic_time_ns = tick.saturating_mul(100_000_000);
            let seconds = monotonic_time_ns as f64 / 1_000_000_000.0;
            let values = simulated_values(seconds);
            let mut samples = vec![simulated_sample(
                StreamKind::RespirationForce,
                tick.saturating_add(1),
                monotonic_time_ns,
                self.wall_time_epoch_ns,
                values.respiration_force,
                "device",
            )];
            latest_force = Some(values.respiration_force);
            if tick.is_multiple_of(10) {
                let heart_sequence = tick / 10 + 1;
                samples.extend([
                    simulated_sample(
                        StreamKind::SkinResistance,
                        heart_sequence,
                        monotonic_time_ns,
                        self.wall_time_epoch_ns,
                        values.skin_resistance_ohms,
                        "ohm",
                    ),
                    simulated_sample(
                        StreamKind::ThoughtStreamAdc,
                        heart_sequence,
                        monotonic_time_ns,
                        self.wall_time_epoch_ns,
                        values.thoughtstream_adc,
                        "count",
                    ),
                    simulated_sample(
                        StreamKind::HeartRate,
                        heart_sequence,
                        monotonic_time_ns,
                        self.wall_time_epoch_ns,
                        values.heart_rate_bpm,
                        "bpm",
                    ),
                    simulated_sample(
                        StreamKind::RrInterval,
                        heart_sequence,
                        monotonic_time_ns,
                        self.wall_time_epoch_ns,
                        values.rr_interval_us,
                        "us",
                    ),
                ]);
            }
            self.model.apply_batch(SampleBatch {
                samples,
                gaps: Vec::new(),
                service_batch_sequence: tick.saturating_add(1),
            });
        }
        self.next_tick = elapsed_tick.saturating_add(1);
        latest_force
    }
}

fn simulated_sample(
    stream: StreamKind,
    sequence: u64,
    monotonic_time_ns: u64,
    wall_time_epoch_ns: u64,
    value: f64,
    unit: &str,
) -> Sample {
    Sample {
        stream: stream as i32,
        source_id: "simulated:in-app".to_owned(),
        sequence,
        monotonic_time_ns,
        wall_time_unix_ns: wall_time_epoch_ns.saturating_add(monotonic_time_ns),
        device_time_ns: None,
        value,
        unit: unit.to_owned(),
        quality_flags: quality::SIMULATED,
    }
}

fn unix_time_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u128::from(u64::MAX)) as u64
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BreathDirection {
    Unknown,
    Inhaling,
    Exhaling,
}

#[derive(Debug, Clone, Copy)]
struct KasinaAnimationFrame {
    expansion: f32,
    layer_rotation_phases: [f32; 4],
    breath_generation: u32,
    breath_layer_progress: f32,
    inhaling: bool,
}

#[derive(Debug)]
struct BreathKasinaState {
    raw_force: Option<f64>,
    low_force: f64,
    high_force: f64,
    target_expansion: f32,
    displayed_expansion: f32,
    trend: f64,
    samples_seen: u64,
    last_animation: Instant,
    layer_rotation_phases: [f32; 4],
    breath_generation: u32,
    breath_layer_progress: f32,
    inhale_start_expansion: f32,
    pending_breath_generation: bool,
    breath_direction: BreathDirection,
    direction_streak: i8,
}

impl BreathKasinaState {
    fn new(now: Instant) -> Self {
        Self {
            raw_force: None,
            low_force: 0.0,
            high_force: 0.0,
            target_expansion: 0.5,
            displayed_expansion: 0.5,
            trend: 0.0,
            samples_seen: 0,
            last_animation: now,
            layer_rotation_phases: [0.0; 4],
            breath_generation: 0,
            breath_layer_progress: 1.0,
            inhale_start_expansion: 0.0,
            pending_breath_generation: false,
            breath_direction: BreathDirection::Unknown,
            direction_streak: 0,
        }
    }

    fn reset(&mut self) {
        self.raw_force = None;
        self.low_force = 0.0;
        self.high_force = 0.0;
        self.target_expansion = 0.5;
        self.displayed_expansion = 0.5;
        self.trend = 0.0;
        self.samples_seen = 0;
        self.breath_direction = BreathDirection::Unknown;
        self.direction_streak = 0;
        self.pending_breath_generation = false;
    }

    fn observe(&mut self, force: f64) {
        if !force.is_finite() {
            return;
        }
        self.samples_seen = self.samples_seen.saturating_add(1);
        let Some(previous_force) = self.raw_force else {
            self.raw_force = Some(force);
            self.low_force = force - MINIMUM_FORCE_SPAN * 0.5;
            self.high_force = force + MINIMUM_FORCE_SPAN * 0.5;
            return;
        };

        let previous_span = (self.high_force - self.low_force).max(MINIMUM_FORCE_SPAN);
        let normalized_change = ((force - previous_force) / previous_span).clamp(-1.0, 1.0);
        self.trend = self.trend.mul_add(0.72, normalized_change * 0.28);

        if force < self.low_force {
            self.low_force = force;
        } else {
            self.low_force += (force - self.low_force) * BREATH_ENVELOPE_RELAXATION;
        }
        if force > self.high_force {
            self.high_force = force;
        } else {
            self.high_force += (force - self.high_force) * BREATH_ENVELOPE_RELAXATION;
        }

        let scale_floor = (force.abs() * 0.0005).max(MINIMUM_FORCE_SPAN);
        let span = self.high_force - self.low_force;
        if span < scale_floor {
            let center = (self.high_force + self.low_force) * 0.5;
            self.low_force = center - scale_floor * 0.5;
            self.high_force = center + scale_floor * 0.5;
        }
        let normalized =
            ((force - self.low_force) / (self.high_force - self.low_force)).clamp(0.0, 1.0);
        self.target_expansion = (0.08 + normalized * 0.84) as f32;
        self.raw_force = Some(force);
        self.update_breath_direction(normalized_change);
    }

    fn advance(&mut self, elapsed: Duration) -> f32 {
        let elapsed_seconds = elapsed.as_secs_f32().min(0.25);
        let smoothing = 1.0 - (-elapsed_seconds / 0.16).exp();
        self.displayed_expansion += (self.target_expansion - self.displayed_expansion) * smoothing;
        self.displayed_expansion
    }

    fn animated_frame(&mut self, now: Instant, visual: &dyn KasinaVisual) -> KasinaAnimationFrame {
        let elapsed = now.saturating_duration_since(self.last_animation);
        self.last_animation = now;
        let expansion = self.advance(elapsed);
        self.advance_breath_layer(elapsed, expansion);
        let layer_rotation_phases = self.advance_layer_rotations(elapsed, expansion, visual);
        KasinaAnimationFrame {
            expansion,
            layer_rotation_phases,
            breath_generation: self.breath_generation,
            breath_layer_progress: self.breath_layer_progress,
            inhaling: self.breath_direction == BreathDirection::Inhaling,
        }
    }

    fn advance_breath_layer(&mut self, elapsed: Duration, expansion: f32) {
        match self.breath_direction {
            BreathDirection::Inhaling => {
                if self.pending_breath_generation {
                    self.breath_layer_progress = (self.breath_layer_progress
                        + elapsed.as_secs_f32().min(0.25) / 0.75)
                        .min(1.0);
                    if self.breath_layer_progress >= 1.0 {
                        self.start_new_breath_layer(expansion);
                    }
                    return;
                }
                let available_span = (0.92 - self.inhale_start_expansion).max(0.10);
                let progress =
                    ((expansion - self.inhale_start_expansion) / available_span).clamp(0.0, 1.0);
                self.breath_layer_progress = self.breath_layer_progress.max(progress);
            }
            BreathDirection::Exhaling => {
                self.breath_layer_progress =
                    (self.breath_layer_progress + elapsed.as_secs_f32().min(0.25) / 0.75).min(1.0);
            }
            BreathDirection::Unknown => {}
        }
    }

    fn start_new_breath_layer(&mut self, expansion: f32) {
        self.breath_generation = self.breath_generation.wrapping_add(1);
        self.breath_layer_progress = 0.0;
        self.inhale_start_expansion = expansion;
        self.pending_breath_generation = false;
    }

    #[cfg(test)]
    fn breath_history_position(&self) -> f64 {
        f64::from(self.breath_generation) + f64::from(self.breath_layer_progress) - 1.0
    }

    fn update_breath_direction(&mut self, normalized_change: f64) {
        if self.samples_seen < 12 {
            return;
        }
        self.direction_streak = if normalized_change > 0.003 {
            if self.direction_streak > 0 {
                self.direction_streak.saturating_add(1).min(3)
            } else {
                1
            }
        } else if normalized_change < -0.003 {
            if self.direction_streak < 0 {
                self.direction_streak.saturating_sub(1).max(-3)
            } else {
                -1
            }
        } else {
            0
        };
        let next_direction = if self.direction_streak >= 2 {
            Some(BreathDirection::Inhaling)
        } else if self.direction_streak <= -2 {
            Some(BreathDirection::Exhaling)
        } else {
            None
        };
        let Some(next_direction) = next_direction else {
            return;
        };
        if next_direction == BreathDirection::Inhaling
            && self.breath_direction != BreathDirection::Inhaling
        {
            if self.breath_layer_progress >= 1.0 {
                self.start_new_breath_layer(self.displayed_expansion);
            } else {
                self.pending_breath_generation = true;
            }
        }
        self.breath_direction = next_direction;
    }

    fn advance_layer_rotations(
        &mut self,
        elapsed: Duration,
        expansion: f32,
        visual: &dyn KasinaVisual,
    ) -> [f32; 4] {
        let elapsed_seconds = elapsed.as_secs_f32().min(0.25);
        let speeds = visual.layer_speeds(expansion);
        for (phase, speed) in self.layer_rotation_phases.iter_mut().zip(speeds) {
            *phase = (*phase + elapsed_seconds * speed).rem_euclid(1.0);
        }
        self.layer_rotation_phases
    }

    fn motion_label(&self) -> &'static str {
        if self.samples_seen < 12 {
            "Calibrating"
        } else if self.trend > 0.012 {
            "Inhaling · expanding"
        } else if self.trend < -0.012 {
            "Exhaling · contracting"
        } else {
            "Resting"
        }
    }
}

fn layer_rotation_speed_slider(ui: &mut egui::Ui, enabled: bool, speed: &mut f32) -> bool {
    ui.add_enabled(
        enabled,
        egui::Slider::new(
            speed,
            kasina_render::MIN_ROTATIONS_PER_SECOND..=kasina_render::MAX_ROTATIONS_PER_SECOND,
        )
        .logarithmic(true)
        .fixed_decimals(3)
        .suffix(" rot/s"),
    )
    .changed()
}

fn animation_speed_slider(ui: &mut egui::Ui, enabled: bool, speed: &mut f32) -> bool {
    ui.add_enabled(
        enabled,
        egui::Slider::new(
            speed,
            kasina_render::MIN_ROTATIONS_PER_SECOND..=kasina_render::MAX_ROTATIONS_PER_SECOND,
        )
        .logarithmic(true)
        .fixed_decimals(3)
        .suffix(" cyc/s"),
    )
    .changed()
}

fn expansion_speed_slider(ui: &mut egui::Ui, enabled: bool, multiplier: &mut f32) -> bool {
    let response = ui.add_enabled(
        enabled,
        egui::Slider::new(
            multiplier,
            kasina_render::MIN_EXPANSION_SPEED_MULTIPLIER
                ..=kasina_render::MAX_EXPANSION_SPEED_MULTIPLIER,
        )
        .fixed_decimals(2)
        .suffix("×"),
    );
    let changed = response.changed();
    response.on_hover_text("Multiplier applied at full expansion; 1× keeps speed constant");
    changed
}

struct KasinaApp {
    endpoint: String,
    service_host: service_host::ServiceHost,
    service_panel: service_panel::ServicePanel,
    serial_ports: Option<ThoughtStreamPorts>,
    service_notice: Option<String>,
    service_command_pending: bool,
    last_status_at: Option<Instant>,
    smoke: Option<SmokeTest>,
    _smoke_directory: Option<tempfile::TempDir>,
    view: View,
    events: Receiver<ClientEvent>,
    network_commands: tokio::sync::mpsc::UnboundedSender<NetworkCommand>,
    cancellation: CancellationToken,
    network_thread: Option<JoinHandle<()>>,
    ui_dropped_batches: Arc<AtomicU64>,
    model: ClientModel,
    simulation: SimulationState,
    breath_kasina: BreathKasinaState,
    thoughtstream: thoughtstream::ThoughtStreamPanel,
    settings: AppSettings,
    settings_path: PathBuf,
    settings_writer: SettingsWriter,
    settings_dirty: bool,
    settings_save_deadline: Option<Instant>,
    settings_notice: Option<String>,
    editing_preset_id: u64,
    recording_label: String,
    recording_notes: String,
    recording_command_pending: bool,
    recording_notice: Option<String>,
    renderer: Option<BiofeedbackRenderer>,
    renderer_name: String,
    renderer_backend: String,
    renderer_device_type: String,
    renderer_driver: String,
    renderer_driver_info: String,
    renderer_hardware_accelerated: bool,
    present_mode: String,
    desired_maximum_frame_latency: Option<u32>,
    surface_health: Arc<SurfaceHealth>,
    device_lost: Arc<parking_lot::Mutex<Option<String>>>,
    stress_instances: u32,
    display_refresh_hz: Option<f64>,
    performance_target_hz: Option<f64>,
    fullscreen: bool,
    started: Instant,
    last_frame: Instant,
    frame_interval_stats: FrameStats,
    ui_cpu_stats: FrameStats,
    benchmark: Option<RenderBenchmark>,
    benchmark_status: Option<String>,
}

struct SmokeTest {
    duration: Duration,
    output: PathBuf,
    frames: u64,
    finished: bool,
}

impl KasinaApp {
    fn new(
        creation_context: &eframe::CreationContext<'_>,
        mut args: Args,
        surface_health: Arc<SurfaceHealth>,
    ) -> Result<Self> {
        let smoke_directory = args
            .smoke_test_seconds
            .map(|_| tempfile::tempdir())
            .transpose()?;
        let mut service_options = kasina_service::ServiceOptions::default();
        if let Some(directory) = &smoke_directory {
            let reservation = std::net::TcpListener::bind("127.0.0.1:0")?;
            let port = reservation.local_addr()?.port();
            args.endpoint = format!("http://127.0.0.1:{port}");
            args.token_path = Some(directory.path().join("service-token"));
            args.service_mode = ServiceMode::Embedded;
            service_options = kasina_service::ServiceOptions {
                port,
                source: kasina_service::ServiceSource::Simulated,
                token_path: args.token_path.clone(),
                lock_path: Some(directory.path().join("service.lock")),
                recordings_dir: Some(directory.path().join("sessions")),
                device_settings_path: Some(directory.path().join("devices.json")),
                ..service_options
            };
        }
        let token_path = args.token_path.clone().unwrap_or(default_token_path()?);
        let settings_path = match &smoke_directory {
            Some(directory) => directory.path().join("app-settings.json"),
            None => default_settings_path()?,
        };
        let (settings, settings_notice) = match AppSettings::load(&settings_path) {
            Ok(settings) => (settings, None),
            Err(error) => {
                warn!(%error, path = %settings_path.display(), "using default app settings");
                (
                    AppSettings::default(),
                    Some(format!("Could not load saved settings: {error}")),
                )
            }
        };
        let editing_preset_id = settings.active_preset_id;
        let settings_writer = SettingsWriter::spawn(settings_path.clone())?;
        let renderer = creation_context
            .wgpu_render_state
            .as_ref()
            .map(BiofeedbackRenderer::new);
        let adapter_info = creation_context
            .wgpu_render_state
            .as_ref()
            .map(|state| state.adapter.get_info());
        let renderer_name = adapter_info
            .as_ref()
            .map_or_else(|| "unavailable".to_owned(), |info| info.name.clone());
        let renderer_backend = adapter_info.as_ref().map_or_else(
            || "unavailable".to_owned(),
            |info| format!("{:?}", info.backend),
        );
        let renderer_device_type = adapter_info.as_ref().map_or_else(
            || "unavailable".to_owned(),
            |info| format!("{:?}", info.device_type),
        );
        let renderer_driver = adapter_info
            .as_ref()
            .map_or_else(|| "unavailable".to_owned(), |info| info.driver.clone());
        let renderer_driver_info = adapter_info
            .as_ref()
            .map_or_else(|| "unavailable".to_owned(), |info| info.driver_info.clone());
        let renderer_hardware_accelerated = adapter_info.as_ref().is_some_and(|info| {
            matches!(
                info.device_type,
                eframe::wgpu::DeviceType::IntegratedGpu | eframe::wgpu::DeviceType::DiscreteGpu
            )
        });
        let surface_config = creation_context
            .wgpu_render_state
            .as_ref()
            .map(|state| state.surface_config);
        let present_mode = surface_config.map_or_else(
            || "unavailable".to_owned(),
            |config| format!("{:?}", config.present_mode),
        );
        let desired_maximum_frame_latency =
            surface_config.and_then(|config| config.desired_maximum_frame_latency);
        let device_lost = Arc::new(parking_lot::Mutex::new(None));
        if let Some(render_state) = &creation_context.wgpu_render_state {
            let device_lost_state = Arc::clone(&device_lost);
            let repaint = creation_context.egui_ctx.clone();
            render_state
                .device
                .set_device_lost_callback(move |reason, message| {
                    let detail = format!("{reason:?}: {message}");
                    tracing::error!(%detail, "wgpu device lost");
                    *device_lost_state.lock() = Some(detail);
                    repaint.request_repaint();
                });
        }
        let (sender, events) = std::sync::mpsc::sync_channel(UI_EVENT_CAPACITY);
        let (network_commands, command_receiver) = tokio::sync::mpsc::unbounded_channel();
        let cancellation = CancellationToken::new();
        let ui_dropped_batches = Arc::new(AtomicU64::new(0));
        let launch_mode = match args.service_mode {
            ServiceMode::Embedded => service_host::LaunchMode::Embedded,
            ServiceMode::Auto if !cfg!(target_os = "linux") => service_host::LaunchMode::Embedded,
            _ => service_host::LaunchMode::Separate,
        };
        let allow_start = !matches!(args.service_mode, ServiceMode::External)
            && ((args.endpoint == "http://127.0.0.1:18861" && args.token_path.is_none())
                || smoke_directory.is_some());
        let service_host = service_host::ServiceHost::new(
            service_host::HostConfig {
                mode: launch_mode,
                options: service_options,
                endpoint: args.endpoint.clone(),
                token_path: token_path.clone(),
                allow_start,
                auto_start: launch_mode == service_host::LaunchMode::Embedded
                    && args.render_benchmark_seconds.is_none()
                    && !settings.simulation_mode,
            },
            creation_context.egui_ctx.clone(),
        )?;
        let network_thread = Some(spawn_network_thread(
            args.endpoint.clone(),
            token_path,
            sender,
            cancellation.clone(),
            Arc::clone(&ui_dropped_batches),
            creation_context.egui_ctx.clone(),
            command_receiver,
        ));
        let now = Instant::now();
        let benchmark = args.render_benchmark_seconds.map(|duration| {
            RenderBenchmark::new(
                now,
                args.benchmark_warmup_seconds,
                duration,
                args.benchmark_output
                    .clone()
                    .unwrap_or_else(|| PathBuf::from("render-benchmark.json")),
            )
        });
        Ok(Self {
            endpoint: args.endpoint,
            service_host,
            service_panel: service_panel::ServicePanel::default(),
            serial_ports: None,
            service_notice: None,
            service_command_pending: false,
            last_status_at: None,
            smoke: args.smoke_test_seconds.map(|seconds| SmokeTest {
                duration: Duration::from_secs_f64(seconds),
                output: args
                    .smoke_test_output
                    .clone()
                    .expect("required smoke output"),
                frames: 0,
                finished: false,
            }),
            _smoke_directory: smoke_directory,
            view: if benchmark.is_some() {
                View::Visualizer
            } else if args.smoke_test_seconds.is_some() || !cfg!(target_os = "linux") {
                View::Service
            } else if settings.visible_tabs.breath_kasina {
                View::BreathKasina
            } else {
                View::Settings
            },
            events,
            network_commands,
            cancellation,
            network_thread,
            ui_dropped_batches,
            model: ClientModel::default(),
            simulation: SimulationState::new(now),
            breath_kasina: BreathKasinaState::new(now),
            thoughtstream: thoughtstream::ThoughtStreamPanel::new(now),
            settings,
            settings_path,
            settings_writer,
            settings_dirty: false,
            settings_save_deadline: None,
            settings_notice,
            editing_preset_id,
            recording_label: "Biofeedback session".to_owned(),
            recording_notes: String::new(),
            recording_command_pending: false,
            recording_notice: None,
            renderer,
            renderer_name,
            renderer_backend,
            renderer_device_type,
            renderer_driver,
            renderer_driver_info,
            renderer_hardware_accelerated,
            present_mode,
            desired_maximum_frame_latency,
            surface_health,
            device_lost,
            stress_instances: args.stress_instances,
            display_refresh_hz: args.display_refresh_hz,
            performance_target_hz: args.performance_target_hz,
            fullscreen: false,
            started: now,
            last_frame: now,
            frame_interval_stats: FrameStats::default(),
            ui_cpu_stats: FrameStats::default(),
            benchmark,
            benchmark_status: None,
        })
    }

    fn drain_events(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                ClientEvent::Connection(connection) => self.model.connection = connection,
                ClientEvent::Connected(connected) => {
                    self.model.connected = connected;
                    // Controls belong to one connection. Even if its disconnect
                    // event was lost to backpressure, a fresh session clears them.
                    self.service_command_pending = false;
                    self.recording_command_pending = false;
                    if connected {
                        self.service_notice = None;
                    }
                    if !connected {
                        self.last_status_at = None;
                        self.serial_ports = None;
                    }
                }
                ClientEvent::ServiceInfo(info) => {
                    self.service_host.observed(info.instance_id.clone());
                    self.model.apply_service_info(info);
                }
                ClientEvent::Status(status) => {
                    self.last_status_at = Some(Instant::now());
                    if let Some(recording) = status.recording.clone() {
                        self.model.recording = Some(recording);
                    }
                    self.model.status = Some(status);
                }
                ClientEvent::Samples(batch) => {
                    if !self.settings.simulation_mode {
                        for sample in &batch.samples {
                            if sample.stream == StreamKind::RespirationForce as i32 {
                                self.breath_kasina.observe(sample.value);
                            }
                        }
                    }
                    self.model.apply_batch(batch);
                }
                ClientEvent::Recording(recording) => {
                    self.recording_command_pending = false;
                    self.service_notice = Some(recording.detail.clone());
                    self.recording_notice = Some(recording.detail.clone());
                    self.model.recording = Some(recording);
                }
                ClientEvent::RecordingError(error) => {
                    self.recording_command_pending = false;
                    self.service_notice = Some(error.clone());
                    self.recording_notice = Some(error);
                }
                ClientEvent::SerialPorts(ports) => self.serial_ports = Some(ports),
                ClientEvent::ServiceCommandComplete(notice) => {
                    self.service_command_pending = false;
                    self.service_notice = notice;
                }
            }
        }
    }

    fn active_model(&self) -> &ClientModel {
        if self.settings.simulation_mode {
            &self.simulation.model
        } else {
            &self.model
        }
    }

    fn update_simulation(&mut self, now: Instant) {
        if self.settings.simulation_mode
            && let Some(force) = self.simulation.update(now)
        {
            self.breath_kasina.observe(force);
        }
    }

    fn set_simulation_mode(&mut self, enabled: bool, now: Instant) {
        if self.settings.simulation_mode == enabled {
            return;
        }
        self.thoughtstream.deactivate(now);
        self.settings.simulation_mode = enabled;
        self.breath_kasina.reset();
        if enabled {
            self.simulation.restart(now);
            self.update_simulation(now);
        } else {
            let recent_force: Vec<_> = self
                .model
                .samples
                .get(&(StreamKind::RespirationForce as i32))
                .into_iter()
                .flatten()
                .rev()
                .take(120)
                .map(|sample| sample.value)
                .collect();
            for force in recent_force.into_iter().rev() {
                self.breath_kasina.observe(force);
            }
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        let model = self.active_model();
        let recording_active = self.model.recording.as_ref().is_some_and(|recording| {
            RecordingState::try_from(recording.state) == Ok(RecordingState::Recording)
        });
        egui::Panel::top("top_bar").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("newKasina");
                ui.separator();
                if self.settings.simulation_mode {
                    ui.colored_label(
                        egui::Color32::from_rgb(245, 190, 70),
                        "SIMULATION · no sensors required",
                    );
                } else {
                    ui.label(&model.connection);
                }
                ui.separator();
                metric_label(ui, "HR", model.latest(StreamKind::HeartRate), 1.0);
                metric_label(ui, "RR", model.latest(StreamKind::RrInterval), 0.001);
                metric_label(
                    ui,
                    "Breath",
                    model.latest(StreamKind::RespirationForce),
                    1.0,
                );
                if recording_active {
                    ui.separator();
                    ui.colored_label(egui::Color32::from_rgb(240, 65, 75), "● REC");
                }
            });
        });
    }

    fn navigation(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("navigation")
            .resizable(false)
            .default_size(162.0)
            .show(ui, |ui| {
                ui.add_space(8.0);
                if ui
                    .selectable_label(self.view == View::Service, "Measurement service")
                    .clicked()
                {
                    self.view = View::Service;
                }
                ui.add_space(5.0);
                ui.separator();
                ui.add_space(5.0);
                let visibility = &self.settings.visible_tabs;
                let mut tabs = Vec::with_capacity(7);
                if visibility.dashboard {
                    tabs.push((View::Dashboard, "Dashboard"));
                }
                if visibility.raw_signals {
                    tabs.push((View::Raw, "Raw signals"));
                }
                if visibility.breath_kasina {
                    tabs.push((View::BreathKasina, "Breath kasina"));
                }
                if visibility.thoughtstream {
                    tabs.push((View::ThoughtStream, "ThoughtStream"));
                }
                if visibility.gpu_stress_test {
                    tabs.push((View::Visualizer, "GPU stress test"));
                }
                if visibility.diagnostics {
                    tabs.push((View::Diagnostics, "Diagnostics"));
                }
                tabs.push((View::Settings, "Settings"));
                for (view, label) in tabs {
                    if ui.selectable_label(self.view == view, label).clicked() {
                        self.view = view;
                    }
                }
            });
    }

    fn service_connected(&self) -> bool {
        self.model.connected
            && self
                .last_status_at
                .is_some_and(|time| time.elapsed() < Duration::from_secs(4))
    }

    fn check_smoke_test(&mut self, context: &egui::Context) {
        let Some(smoke) = self.smoke.as_mut() else {
            return;
        };
        if smoke.finished {
            return;
        }
        smoke.frames += 1;
        context.request_repaint_after(Duration::from_millis(50));
        let host = self.service_host.status();
        let samples = self
            .model
            .samples
            .values()
            .map(VecDeque::len)
            .sum::<usize>();
        let instance = self
            .model
            .service_info
            .as_ref()
            .map(|info| info.instance_id.as_str());
        let success = self.started.elapsed() >= smoke.duration
            && smoke.frames >= 2
            && samples > 0
            && self.model.connected
            && host.phase == service_host::Phase::Owned
            && instance == host.instance_id.as_deref();
        let timeout = self.started.elapsed() > smoke.duration + Duration::from_secs(20);
        if !success && !timeout && host.phase != service_host::Phase::Failed {
            return;
        }
        let report = serde_json::json!({
            "success": success,
            "samples_received": samples,
            "ui_frames": smoke.frames,
            "service_instance": instance,
            "embedded_instance": host.instance_id,
            "platform": std::env::consts::OS,
            "detail": if success { "The UI received live samples from its isolated embedded service".to_owned() } else { host.notice.unwrap_or_else(|| self.model.connection.clone()) },
        });
        match serde_json::to_vec_pretty(&report)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| fs::write(&smoke.output, bytes).map_err(anyhow::Error::from))
        {
            Ok(()) => smoke.finished = true,
            Err(error) => {
                tracing::error!(%error, "write smoke result");
                smoke.finished = true;
            }
        }
        context.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn measurement_service(&mut self, ui: &mut egui::Ui) {
        use service_host::Phase;
        use service_panel::{ServiceAction, ServiceMode as PanelMode};
        let host = self.service_host.status();
        let connected = self.service_connected();
        let owned = host.phase == Phase::Owned
            && host.instance_id.as_deref()
                == self
                    .model
                    .service_info
                    .as_ref()
                    .map(|info| info.instance_id.as_str());
        let mode = if matches!(host.phase, Phase::Stopping) {
            PanelMode::Stopping
        } else if connected {
            if owned {
                PanelMode::Embedded
            } else {
                PanelMode::External
            }
        } else {
            match host.phase {
                Phase::Starting | Phase::Owned => PanelMode::Starting,
                Phase::Failed => PanelMode::Failed,
                _ => PanelMode::Offline,
            }
        };
        let controls_available = self
            .model
            .service_info
            .as_ref()
            .is_some_and(|info| info.protocol_minor >= 3);
        let device_controls = self
            .model
            .status
            .as_ref()
            .into_iter()
            .flat_map(|status| &status.devices)
            .filter(|device| device.connection_control_available)
            .filter_map(|device| {
                device
                    .device
                    .as_ref()
                    .map(|info| service_panel::DeviceControl {
                        id: info.id.clone(),
                        enabled: device.connection_enabled,
                    })
            })
            .collect::<Vec<_>>();
        let serial = self
            .serial_ports
            .as_ref()
            .map(|ports| service_panel::SerialSettings {
                automatic: ports.selected_port.is_empty(),
                selected_path: (!ports.selected_port.is_empty())
                    .then(|| ports.selected_port.clone()),
                ports: ports
                    .ports
                    .iter()
                    .map(|port| service_panel::SerialChoice {
                        path: port.name.clone(),
                        label: port.label.clone(),
                    })
                    .collect(),
                detail: String::new(),
            });
        let notice = self
            .service_notice
            .as_deref()
            .or(host.notice.as_deref())
            .or_else(|| {
                (!connected && (host.phase != Phase::Offline || !self.service_host.allow_start))
                    .then_some(self.model.connection.as_str())
            });
        if self.settings.simulation_mode {
            ui.label(egui::RichText::new("Your signal panels are in demo mode. This page shows the actual measurement service.").small().color(egui::Color32::from_rgb(245, 191, 105)));
        }
        let actions = self.service_panel.ui(
            ui,
            service_panel::ServicePanelInput {
                mode,
                connected,
                status: self.model.status.as_ref(),
                notice,
                controls_available,
                device_controls: &device_controls,
                serial: serial.as_ref(),
                busy: self.service_command_pending
                    || self.recording_command_pending
                    || matches!(mode, PanelMode::Starting | PanelMode::Stopping),
                can_start: self.service_host.allow_start
                    && !connected
                    && host.phase != Phase::Owned,
            },
        );
        for action in actions {
            self.service_notice = None;
            let command = match action {
                ServiceAction::StartService => {
                    self.service_host.start();
                    continue;
                }
                ServiceAction::StopService if owned => {
                    self.service_host.stop();
                    continue;
                }
                ServiceAction::StopService => continue,
                ServiceAction::OpenRecordings => {
                    if let Some(recording) = &self.model.recording
                        && !recording.directory.is_empty()
                        && let Err(error) = open_recordings_folder(&recording.directory)
                    {
                        self.service_notice = Some(format!("Could not open recordings: {error}"));
                    }
                    continue;
                }
                ServiceAction::SetDeviceEnabled { device_id, enabled } => {
                    NetworkCommand::SetDeviceEnabled { device_id, enabled }
                }
                ServiceAction::ReconnectDevice { device_id } => {
                    NetworkCommand::ReconnectDevice { device_id }
                }
                ServiceAction::RefreshSerialPorts => NetworkCommand::RefreshSerialPorts,
                ServiceAction::SetThoughtStreamPort(port) => {
                    NetworkCommand::SetThoughtStreamPort(port)
                }
                ServiceAction::StartRecording { label } => {
                    self.recording_command_pending = true;
                    NetworkCommand::StartRecording {
                        label: if label.trim().is_empty() {
                            "Biofeedback session".to_owned()
                        } else {
                            label
                        },
                        notes: String::new(),
                    }
                }
                ServiceAction::StopRecording => {
                    self.recording_command_pending = true;
                    NetworkCommand::StopRecording
                }
            };
            if !connected {
                self.service_notice =
                    Some("The service disconnected. Reconnect and try again.".to_owned());
                self.recording_command_pending = false;
                continue;
            }
            self.service_command_pending = !self.recording_command_pending;
            if self.network_commands.send(command).is_err() {
                self.service_command_pending = false;
                self.recording_command_pending = false;
                self.service_notice = Some("The service connection is unavailable.".to_owned());
            }
        }
    }

    fn dashboard(&self, ui: &mut egui::Ui) {
        let model = self.active_model();
        ui.heading("Live biofeedback");
        ui.label(if self.settings.simulation_mode {
            "Displaying deterministic synthetic signals generated inside the app."
        } else {
            if self.service_host.status().phase == service_host::Phase::Owned {
                "Measurements run in this app and stop cleanly when it closes."
            } else {
                "Measurements come from the independent sensor service."
            }
        });
        ui.add_space(12.0);
        egui::Grid::new("summary_grid")
            .striped(true)
            .show(ui, |ui| {
                ui.strong("Service endpoint");
                ui.label(&self.endpoint);
                ui.end_row();
                ui.strong("Service instance");
                ui.label(
                    model
                        .service_info
                        .as_ref()
                        .map_or("—", |info| info.instance_id.as_str()),
                );
                ui.end_row();
                ui.strong("Retained UI samples");
                ui.label(
                    model
                        .samples
                        .values()
                        .map(VecDeque::len)
                        .sum::<usize>()
                        .to_string(),
                );
                ui.end_row();
            });
        ui.add_space(16.0);
        draw_signal(
            ui,
            "Respiration force",
            model.samples.get(&(StreamKind::RespirationForce as i32)),
            egui::Color32::from_rgb(80, 190, 220),
            220.0,
        );
    }

    fn raw_signals(&self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| self.raw_signals_content(ui));
    }

    fn raw_signals_content(&self, ui: &mut egui::Ui) {
        let model = self.active_model();
        ui.heading(if self.settings.simulation_mode {
            "Simulated streams"
        } else {
            "Raw service streams"
        });
        draw_signal(
            ui,
            "Respiration force",
            model.samples.get(&(StreamKind::RespirationForce as i32)),
            egui::Color32::from_rgb(80, 190, 220),
            210.0,
        );
        draw_signal(
            ui,
            "Heart rate",
            model.samples.get(&(StreamKind::HeartRate as i32)),
            egui::Color32::from_rgb(230, 92, 116),
            160.0,
        );
        draw_signal(
            ui,
            "RR interval",
            model.samples.get(&(StreamKind::RrInterval as i32)),
            egui::Color32::from_rgb(210, 160, 90),
            160.0,
        );
        draw_signal(
            ui,
            "ThoughtStream · skin resistance (ohm)",
            model.samples.get(&(StreamKind::SkinResistance as i32)),
            egui::Color32::from_rgb(150, 210, 125),
            160.0,
        );
        draw_signal(
            ui,
            "ThoughtStream · raw ADC (count)",
            model.samples.get(&(StreamKind::ThoughtStreamAdc as i32)),
            egui::Color32::from_rgb(175, 145, 220),
            130.0,
        );
    }

    fn breath_kasina(&mut self, ui: &mut egui::Ui) {
        let preset_names: Vec<_> = self
            .settings
            .presets
            .iter()
            .map(|preset| (preset.id, preset.name.clone()))
            .collect();
        let mut selected_preset = self.settings.active_preset_id;
        ui.horizontal(|ui| {
            ui.heading("Breath Kasina");
            ui.separator();
            ui.colored_label(
                egui::Color32::from_rgb(112, 214, 224),
                self.breath_kasina.motion_label(),
            );
            if let Some(force) = self.breath_kasina.raw_force {
                ui.separator();
                ui.label(format!("Force {force:.2}"));
            }
            ui.separator();
            egui::ComboBox::from_id_salt("active_kasina_preset")
                .selected_text(&self.settings.active_preset().name)
                .show_ui(ui, |ui| {
                    for (id, name) in &preset_names {
                        ui.selectable_value(&mut selected_preset, *id, name);
                    }
                });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("Reset breathing range").clicked() {
                    self.breath_kasina.reset();
                }
            });
        });
        if selected_preset != self.settings.active_preset_id {
            self.settings.active_preset_id = selected_preset;
            self.editing_preset_id = selected_preset;
            self.mark_settings_changed(ui.ctx());
        }
        let active_visual = self.settings.active_preset().visual.clone();
        let animation = self
            .breath_kasina
            .animated_frame(Instant::now(), active_visual.as_visual());
        ui.label("The kasina follows the respiration belt directly: rising force expands it.");
        ui.add_space(6.0);
        let size = egui::vec2(ui.available_width(), ui.available_height().max(180.0));
        if let Some(renderer) = &self.renderer {
            renderer.paint_breath_kasina(
                ui,
                size,
                active_visual.as_visual(),
                KasinaAnimationInput {
                    layer_rotation_phases: animation.layer_rotation_phases,
                    respiration: animation.expansion,
                    breath_generation: animation.breath_generation,
                    breath_layer_progress: animation.breath_layer_progress,
                    inhaling: animation.inhaling,
                },
            );
        } else {
            let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
            ui.painter()
                .rect_filled(rect, 8.0, egui::Color32::from_rgb(3, 6, 20));
            let (minimum_radius, maximum_radius) = match &active_visual {
                KasinaVisualPreset::LuminousMandala(options) => {
                    (options.minimum_radius, options.maximum_radius)
                }
                KasinaVisualPreset::AuroraVortex(options) => {
                    (options.minimum_radius, options.maximum_radius)
                }
                KasinaVisualPreset::OrganicKaleidoscope(options) => {
                    (options.seed_radius, options.completed_layer_width)
                }
                KasinaVisualPreset::PaperDisk(options) => {
                    (options.minimum_radius, options.maximum_radius)
                }
            };
            let radius = rect.width().min(rect.height())
                * (minimum_radius + (maximum_radius - minimum_radius) * animation.expansion)
                * 0.5;
            for ring in 1..=6 {
                let fraction = ring as f32 / 6.0;
                ui.painter().circle_stroke(
                    rect.center(),
                    radius * fraction,
                    egui::Stroke::new(
                        1.0 + (1.0 - fraction) * 2.0,
                        egui::Color32::from_rgb(
                            (80.0 + fraction * 120.0) as u8,
                            (80.0 + (1.0 - fraction) * 130.0) as u8,
                            220,
                        ),
                    ),
                );
            }
        }
    }

    fn settings(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| self.settings_content(ui));
    }

    fn settings_content(&mut self, ui: &mut egui::Ui) {
        ui.heading("Settings");
        if let Some(notice) = &self.settings_notice {
            ui.colored_label(egui::Color32::YELLOW, notice);
        }
        ui.label(format!("Saved to {}", self.settings_path.display()));
        ui.add_space(12.0);

        let mut changed = false;
        ui.heading("Data source");
        let mut simulation_mode = self.settings.simulation_mode;
        if ui
            .checkbox(
                &mut simulation_mode,
                "Simulation mode (no wearable devices required)",
            )
            .changed()
        {
            self.set_simulation_mode(simulation_mode, Instant::now());
            changed = true;
        }
        ui.label(
            "Generates a regular 10-second breathing wave at 10 Hz, plus heart rate, RR intervals, and ThoughtStream resistance/ADC at 1 Hz. The measurement service keeps running unchanged.",
        );
        ui.add_space(18.0);
        ui.separator();
        ui.add_space(12.0);

        ui.heading("Session recording");
        let recording_status = self.model.recording.clone();
        let recording = recording_status.clone().unwrap_or_default();
        let recording_state = RecordingState::try_from(recording.state).unwrap_or_default();
        let recording_active = recording_state == RecordingState::Recording;
        let recording_available = recording_status.is_some()
            && !matches!(
                recording_state,
                RecordingState::Unspecified | RecordingState::Unavailable
            );
        egui::Grid::new("session_recording_controls")
            .num_columns(2)
            .spacing([18.0, 8.0])
            .show(ui, |ui| {
                ui.strong("Session label");
                ui.add_enabled(
                    !recording_active,
                    egui::TextEdit::singleline(&mut self.recording_label)
                        .char_limit(100)
                        .desired_width(320.0),
                );
                ui.end_row();
                ui.strong("Notes");
                ui.add_enabled(
                    !recording_active,
                    egui::TextEdit::multiline(&mut self.recording_notes)
                        .char_limit(4_000)
                        .desired_rows(2)
                        .desired_width(420.0),
                );
                ui.end_row();
                ui.strong("Status");
                ui.label(format!(
                    "{} · {} samples · {} dropped",
                    recording_state_label(recording_state),
                    recording.sample_count,
                    recording.dropped_samples
                ));
                ui.end_row();
                if !recording.directory.is_empty() {
                    ui.strong("Directory");
                    ui.label(&recording.directory);
                    ui.end_row();
                }
            });
        ui.horizontal(|ui| {
            let can_start = recording_available
                && !recording_active
                && !self.recording_command_pending
                && !self.recording_label.trim().is_empty();
            if ui
                .add_enabled(can_start, egui::Button::new("Start recording"))
                .clicked()
            {
                let command = NetworkCommand::StartRecording {
                    label: self.recording_label.trim().to_owned(),
                    notes: self.recording_notes.trim().to_owned(),
                };
                if self.network_commands.send(command).is_ok() {
                    self.recording_command_pending = true;
                    self.recording_notice = Some("Starting recording…".to_owned());
                } else {
                    self.recording_notice = Some("Network task is not running".to_owned());
                }
            }
            if ui
                .add_enabled(
                    recording_active && !self.recording_command_pending,
                    egui::Button::new("Stop recording"),
                )
                .clicked()
            {
                if self
                    .network_commands
                    .send(NetworkCommand::StopRecording)
                    .is_ok()
                {
                    self.recording_command_pending = true;
                    self.recording_notice = Some("Stopping and syncing recording…".to_owned());
                } else {
                    self.recording_notice = Some("Network task is not running".to_owned());
                }
            }
            if self.recording_command_pending {
                ui.spinner();
            }
        });
        if let Some(notice) = &self.recording_notice {
            ui.label(notice);
        } else if !recording.detail.is_empty() {
            ui.label(&recording.detail);
        }
        if self.settings.simulation_mode {
            ui.colored_label(
                egui::Color32::from_rgb(245, 190, 70),
                "In-app simulation is display-only. Server recordings contain the service's own live acquisition streams.",
            );
        }
        ui.add_space(18.0);
        ui.separator();
        ui.add_space(12.0);

        ui.heading("Visible tabs");
        ui.label("Choose the tools that appear in the left tab bar.");
        egui::Grid::new("visible_tabs_settings")
            .num_columns(2)
            .spacing([20.0, 6.0])
            .show(ui, |ui| {
                changed |= ui
                    .checkbox(
                        &mut self.settings.visible_tabs.breath_kasina,
                        "Breath kasina",
                    )
                    .changed();
                changed |= ui
                    .checkbox(&mut self.settings.visible_tabs.dashboard, "Dashboard")
                    .changed();
                ui.end_row();
                changed |= ui
                    .checkbox(&mut self.settings.visible_tabs.raw_signals, "Raw signals")
                    .changed();
                changed |= ui
                    .checkbox(&mut self.settings.visible_tabs.diagnostics, "Diagnostics")
                    .changed();
                ui.end_row();
                changed |= ui
                    .checkbox(
                        &mut self.settings.visible_tabs.gpu_stress_test,
                        "GPU stress test",
                    )
                    .changed();
                changed |= ui
                    .checkbox(
                        &mut self.settings.visible_tabs.thoughtstream,
                        "ThoughtStream",
                    )
                    .changed();
                ui.end_row();
                let mut settings_visible = true;
                ui.add_enabled(
                    false,
                    egui::Checkbox::new(&mut settings_visible, "Settings (always visible)"),
                );
                ui.end_row();
            });

        ui.add_space(18.0);
        ui.separator();
        ui.add_space(12.0);
        ui.heading("Kasina presets");
        ui.label(
            "A preset chooses a visual implementation and its implementation-specific options.",
        );

        let preset_names: Vec<_> = self
            .settings
            .presets
            .iter()
            .map(|preset| (preset.id, preset.name.clone()))
            .collect();
        if self.settings.preset(self.editing_preset_id).is_none() {
            self.editing_preset_id = self.settings.active_preset_id;
        }
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("preset_to_edit")
                .selected_text(
                    self.settings
                        .preset(self.editing_preset_id)
                        .map_or("Select preset", |preset| preset.name.as_str()),
                )
                .show_ui(ui, |ui| {
                    for (id, name) in &preset_names {
                        ui.selectable_value(&mut self.editing_preset_id, *id, name);
                    }
                });
            if ui.button("Add preset").clicked() {
                self.editing_preset_id = self.settings.add_preset(self.editing_preset_id);
                changed = true;
            }
            if ui
                .add_enabled(self.settings.presets.len() > 1, egui::Button::new("Remove"))
                .clicked()
            {
                changed |= self.settings.remove_preset(self.editing_preset_id);
                self.editing_preset_id = self.settings.active_preset_id;
            }
            if ui.button("Restore default presets").clicked() {
                let defaults = AppSettings::default();
                self.settings.presets = defaults.presets;
                self.settings.active_preset_id = defaults.active_preset_id;
                self.settings.next_preset_id = defaults.next_preset_id;
                self.editing_preset_id = self.settings.active_preset_id;
                changed = true;
            }
        });

        let mut make_active = false;
        if let Some(preset) = self.settings.preset_mut(self.editing_preset_id) {
            egui::Grid::new("kasina_preset_editor")
                .num_columns(2)
                .spacing([18.0, 8.0])
                .show(ui, |ui| {
                    ui.strong("Preset name");
                    let name_response = ui.text_edit_singleline(&mut preset.name);
                    changed |= name_response.changed();
                    if name_response.lost_focus() && preset.name.trim().is_empty() {
                        preset.name = "Untitled preset".to_owned();
                        changed = true;
                    }
                    if preset.name.chars().count() > 80 {
                        preset.name = preset.name.chars().take(80).collect();
                        changed = true;
                    }
                    ui.end_row();
                    ui.strong("Implementation");
                    let current_implementation = match &preset.visual {
                        KasinaVisualPreset::LuminousMandala(_) => 0,
                        KasinaVisualPreset::AuroraVortex(_) => 1,
                        KasinaVisualPreset::OrganicKaleidoscope(_) => 2,
                        KasinaVisualPreset::PaperDisk(_) => 3,
                    };
                    let mut selected_implementation = current_implementation;
                    egui::ComboBox::from_id_salt("kasina_implementation")
                        .selected_text(preset.visual.implementation_name())
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut selected_implementation,
                                0,
                                "Luminous mandala",
                            );
                            ui.selectable_value(&mut selected_implementation, 1, "Aurora vortex");
                            ui.selectable_value(
                                &mut selected_implementation,
                                2,
                                "Organic kaleidoscope",
                            );
                            ui.selectable_value(&mut selected_implementation, 3, "Paper on wood");
                        });
                    if selected_implementation != current_implementation {
                        preset.visual = match selected_implementation {
                            0 => KasinaVisualPreset::LuminousMandala(LuminousMandala::default()),
                            1 => KasinaVisualPreset::AuroraVortex(
                                kasina_render::AuroraVortex::default(),
                            ),
                            2 => KasinaVisualPreset::OrganicKaleidoscope(
                                OrganicKaleidoscope::default(),
                            ),
                            _ => KasinaVisualPreset::PaperDisk(PaperDisk::default()),
                        };
                        changed = true;
                    }
                    ui.end_row();

                    match &mut preset.visual {
                        KasinaVisualPreset::LuminousMandala(options) => {
                            ui.strong("Minimum radius");
                            changed |= ui
                                .add(
                                    egui::Slider::new(&mut options.minimum_radius, 0.12..=0.80)
                                        .fixed_decimals(2),
                                )
                                .changed();
                            ui.end_row();
                            ui.strong("Maximum radius");
                            changed |= ui
                                .add(
                                    egui::Slider::new(&mut options.maximum_radius, 0.20..=1.00)
                                        .fixed_decimals(2),
                                )
                                .changed();
                            ui.end_row();
                            ui.strong("Rotation");
                            changed |= ui
                                .checkbox(&mut options.rotation_enabled, "Enabled")
                                .changed();
                            ui.end_row();
                            ui.strong("Inner layer speed");
                            changed |= layer_rotation_speed_slider(
                                ui,
                                options.rotation_enabled,
                                &mut options.inner_rotations_per_second,
                            );
                            ui.end_row();
                            ui.strong("Middle layer speed");
                            changed |= layer_rotation_speed_slider(
                                ui,
                                options.rotation_enabled,
                                &mut options.middle_rotations_per_second,
                            );
                            ui.end_row();
                            ui.strong("Third layer speed");
                            changed |= layer_rotation_speed_slider(
                                ui,
                                options.rotation_enabled,
                                &mut options.third_rotations_per_second,
                            );
                            ui.end_row();
                            ui.strong("Gold layer speed");
                            changed |= layer_rotation_speed_slider(
                                ui,
                                options.rotation_enabled,
                                &mut options.gold_rotations_per_second,
                            );
                            ui.end_row();
                            ui.strong("Full-expansion speed");
                            changed |= expansion_speed_slider(
                                ui,
                                options.rotation_enabled,
                                &mut options.expansion_speed_multiplier,
                            );
                            ui.end_row();
                            *options = options.sanitized();
                        }
                        KasinaVisualPreset::AuroraVortex(options) => {
                            ui.strong("Minimum radius");
                            changed |= ui
                                .add(
                                    egui::Slider::new(&mut options.minimum_radius, 0.12..=0.80)
                                        .fixed_decimals(2),
                                )
                                .changed();
                            ui.end_row();
                            ui.strong("Maximum radius");
                            changed |= ui
                                .add(
                                    egui::Slider::new(&mut options.maximum_radius, 0.20..=1.00)
                                        .fixed_decimals(2),
                                )
                                .changed();
                            ui.end_row();
                            ui.strong("Spiral arms");
                            changed |= ui
                                .add(egui::Slider::new(&mut options.arms, 3..=24))
                                .changed();
                            ui.end_row();
                            ui.strong("Spiral twist");
                            changed |= ui
                                .add(
                                    egui::Slider::new(&mut options.twist, 1.0..=14.0)
                                        .fixed_decimals(1),
                                )
                                .changed();
                            ui.end_row();
                            ui.strong("Glow");
                            changed |= ui
                                .add(
                                    egui::Slider::new(&mut options.glow, 0.35..=2.50)
                                        .fixed_decimals(2),
                                )
                                .changed();
                            ui.end_row();
                            ui.strong("Spectral hue");
                            changed |= ui
                                .add(
                                    egui::Slider::new(&mut options.hue, 0.0..=1.0)
                                        .fixed_decimals(2),
                                )
                                .changed();
                            ui.end_row();
                            ui.strong("Rotation");
                            changed |= ui
                                .checkbox(&mut options.rotation_enabled, "Enabled")
                                .changed();
                            ui.end_row();
                            ui.strong("Iris speed");
                            changed |= layer_rotation_speed_slider(
                                ui,
                                options.rotation_enabled,
                                &mut options.iris_rotations_per_second,
                            );
                            ui.end_row();
                            ui.strong("Filament speed");
                            changed |= layer_rotation_speed_slider(
                                ui,
                                options.rotation_enabled,
                                &mut options.filament_rotations_per_second,
                            );
                            ui.end_row();
                            ui.strong("Halo speed");
                            changed |= layer_rotation_speed_slider(
                                ui,
                                options.rotation_enabled,
                                &mut options.halo_rotations_per_second,
                            );
                            ui.end_row();
                            ui.strong("Spark speed");
                            changed |= layer_rotation_speed_slider(
                                ui,
                                options.rotation_enabled,
                                &mut options.spark_rotations_per_second,
                            );
                            ui.end_row();
                            ui.strong("Full-expansion speed");
                            changed |= expansion_speed_slider(
                                ui,
                                options.rotation_enabled,
                                &mut options.expansion_speed_multiplier,
                            );
                            ui.end_row();
                            *options = options.sanitized();
                        }
                        KasinaVisualPreset::OrganicKaleidoscope(options) => {
                            ui.strong("Newborn seed size");
                            changed |= ui
                                .add(
                                    egui::Slider::new(&mut options.seed_radius, 0.005..=0.15)
                                        .fixed_decimals(3),
                                )
                                .changed();
                            ui.end_row();
                            ui.strong("Completed layer width");
                            changed |= ui
                                .add(
                                    egui::Slider::new(
                                        &mut options.completed_layer_width,
                                        0.06..=0.35,
                                    )
                                    .fixed_decimals(3),
                                )
                                .changed();
                            ui.end_row();
                            ui.strong("Mirrored sectors");
                            changed |= ui
                                .add(egui::Slider::new(&mut options.sectors, 4..=32))
                                .changed();
                            ui.end_row();
                            ui.strong("Ring density");
                            changed |= ui
                                .add(
                                    egui::Slider::new(&mut options.ring_density, 2.0..=14.0)
                                        .fixed_decimals(2),
                                )
                                .changed();
                            ui.end_row();
                            ui.strong("Organic warp");
                            changed |= ui
                                .add(
                                    egui::Slider::new(&mut options.warp, 0.0..=1.50)
                                        .fixed_decimals(2),
                                )
                                .changed();
                            ui.end_row();
                            ui.strong("Palette hue");
                            changed |= ui
                                .add(
                                    egui::Slider::new(&mut options.hue, 0.0..=1.0)
                                        .fixed_decimals(2),
                                )
                                .changed();
                            ui.end_row();
                            ui.strong("Animation");
                            changed |= ui
                                .checkbox(&mut options.animation_enabled, "Enabled")
                                .changed();
                            ui.end_row();
                            ui.strong("Geometry speed");
                            changed |= animation_speed_slider(
                                ui,
                                options.animation_enabled,
                                &mut options.geometry_rotations_per_second,
                            );
                            ui.end_row();
                            ui.strong("Morph speed");
                            changed |= animation_speed_slider(
                                ui,
                                options.animation_enabled,
                                &mut options.morph_rotations_per_second,
                            );
                            ui.end_row();
                            ui.strong("Palette speed");
                            changed |= animation_speed_slider(
                                ui,
                                options.animation_enabled,
                                &mut options.palette_rotations_per_second,
                            );
                            ui.end_row();
                            ui.strong("Warp speed");
                            changed |= animation_speed_slider(
                                ui,
                                options.animation_enabled,
                                &mut options.warp_rotations_per_second,
                            );
                            ui.end_row();
                            ui.strong("Full-expansion speed");
                            changed |= expansion_speed_slider(
                                ui,
                                options.animation_enabled,
                                &mut options.expansion_speed_multiplier,
                            );
                            ui.end_row();
                            *options = options.sanitized();
                        }
                        KasinaVisualPreset::PaperDisk(options) => {
                            ui.strong("Minimum radius");
                            changed |= ui
                                .add(
                                    egui::Slider::new(&mut options.minimum_radius, 0.12..=0.80)
                                        .fixed_decimals(2),
                                )
                                .changed();
                            ui.end_row();
                            ui.strong("Maximum radius");
                            changed |= ui
                                .add(
                                    egui::Slider::new(&mut options.maximum_radius, 0.20..=1.00)
                                        .fixed_decimals(2),
                                )
                                .changed();
                            ui.end_row();
                            ui.strong("Wood grain scale");
                            changed |= ui
                                .add(
                                    egui::Slider::new(&mut options.wood_grain_scale, 2.0..=16.0)
                                        .fixed_decimals(1),
                                )
                                .changed();
                            ui.end_row();
                            ui.strong("Wood contrast");
                            changed |= ui
                                .add(
                                    egui::Slider::new(&mut options.wood_contrast, 0.0..=1.0)
                                        .fixed_decimals(2),
                                )
                                .changed();
                            ui.end_row();
                            ui.strong("Paper texture");
                            changed |= ui
                                .add(
                                    egui::Slider::new(&mut options.paper_texture, 0.0..=1.0)
                                        .fixed_decimals(2),
                                )
                                .changed();
                            ui.end_row();
                            ui.strong("Shadow strength");
                            changed |= ui
                                .add(
                                    egui::Slider::new(&mut options.shadow_strength, 0.0..=1.0)
                                        .fixed_decimals(2),
                                )
                                .changed();
                            ui.end_row();
                            *options = options.sanitized();
                        }
                    }
                    ui.strong("Preview");
                    make_active = ui.button("Use this preset").clicked();
                    ui.end_row();
                });
        }
        if make_active && self.settings.active_preset_id != self.editing_preset_id {
            self.settings.active_preset_id = self.editing_preset_id;
            changed = true;
        }
        if changed {
            self.settings_notice = None;
            self.mark_settings_changed(ui.ctx());
        }
    }

    fn mark_settings_changed(&mut self, context: &egui::Context) {
        self.settings_dirty = true;
        self.settings_save_deadline = Some(Instant::now() + Duration::from_millis(300));
        context.request_repaint_after(Duration::from_millis(300));
    }

    fn persist_settings_if_due(&mut self, context: &egui::Context, now: Instant) {
        if !self.settings_dirty {
            return;
        }
        let deadline = self.settings_save_deadline.unwrap_or(now);
        if now < deadline {
            context.request_repaint_after(deadline.duration_since(now));
            return;
        }
        if self
            .settings_writer
            .try_queue(self.settings.clone().sanitized())
        {
            self.settings_dirty = false;
            self.settings_save_deadline = None;
        } else {
            self.settings_save_deadline = Some(now + Duration::from_millis(100));
            context.request_repaint_after(Duration::from_millis(100));
        }
    }

    fn visualizer(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Retained wgpu visualizer");
            ui.add(
                egui::Slider::new(&mut self.stress_instances, 100..=100_000)
                    .logarithmic(true)
                    .text("instances"),
            );
        });
        let respiration = self
            .active_model()
            .latest(StreamKind::RespirationForce)
            .map_or(0.5, |sample| {
                ((sample.value - 25.0) / 50.0).clamp(0.0, 1.0) as f32
            });
        let size = egui::vec2(
            ui.available_width(),
            (ui.available_height() - 36.0).max(180.0),
        );
        if let Some(renderer) = &self.renderer {
            renderer.paint(
                ui,
                size,
                self.started.elapsed().as_secs_f32(),
                respiration,
                self.stress_instances,
            );
        } else {
            let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
            ui.painter()
                .rect_filled(rect, 8.0, egui::Color32::from_rgb(18, 24, 34));
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "wgpu renderer unavailable",
                egui::FontId::proportional(18.0),
                egui::Color32::LIGHT_GRAY,
            );
        }
        if let Some(status) = &self.benchmark_status {
            ui.label(status);
        } else if let Some(benchmark) = &self.benchmark {
            let now = Instant::now();
            let status = if now < benchmark.measurement_start {
                format!(
                    "Benchmark warm-up: {:.1} s remaining",
                    benchmark
                        .measurement_start
                        .duration_since(now)
                        .as_secs_f64()
                )
            } else {
                format!(
                    "Benchmark recording: {:.1} s remaining",
                    benchmark
                        .measurement_end
                        .saturating_duration_since(now)
                        .as_secs_f64()
                )
            };
            ui.label(status);
        }
    }

    fn diagnostics(&self, ui: &mut egui::Ui) {
        ui.heading("Diagnostics");
        let prepare = self
            .renderer
            .as_ref()
            .map(BiofeedbackRenderer::prepare_stats)
            .unwrap_or_default();
        egui::Grid::new("diagnostics_grid")
            .striped(true)
            .num_columns(2)
            .show(ui, |ui| {
                diagnostic_row(
                    ui,
                    "Displayed data source",
                    if self.settings.simulation_mode {
                        "in-app simulation"
                    } else {
                        "measurement service"
                    },
                );
                diagnostic_row(ui, "Connection", &self.model.connection);
                diagnostic_row(
                    ui,
                    "GPU",
                    &format!(
                        "{} ({}, {})",
                        self.renderer_name, self.renderer_backend, self.renderer_device_type
                    ),
                );
                diagnostic_row(
                    ui,
                    "Frame interval average",
                    &format!(
                        "{:.3} ms",
                        milliseconds(self.frame_interval_stats.average())
                    ),
                );
                diagnostic_row(
                    ui,
                    "Frame interval p95 / p99",
                    &format!(
                        "{:.3} / {:.3} ms",
                        milliseconds(self.frame_interval_stats.percentile(0.95)),
                        milliseconds(self.frame_interval_stats.percentile(0.99))
                    ),
                );
                diagnostic_row(
                    ui,
                    "UI CPU average",
                    &format!("{:.3} ms", milliseconds(self.ui_cpu_stats.average())),
                );
                diagnostic_row(
                    ui,
                    "UI CPU p95 / p99",
                    &format!(
                        "{:.3} / {:.3} ms",
                        milliseconds(self.ui_cpu_stats.percentile(0.95)),
                        milliseconds(self.ui_cpu_stats.percentile(0.99))
                    ),
                );
                diagnostic_row(
                    ui,
                    "wgpu prepare average",
                    &format!("{:.3} ms", prepare.average().as_secs_f64() * 1_000.0),
                );
                diagnostic_row(
                    ui,
                    "Latest GPU upload",
                    &format!("{} bytes", prepare.uploaded_bytes()),
                );
                diagnostic_row(
                    ui,
                    "Surface outdated / lost",
                    &format!(
                        "{} / {}",
                        self.surface_health.outdated.load(Ordering::Relaxed),
                        self.surface_health.lost.load(Ordering::Relaxed)
                    ),
                );
                diagnostic_row(
                    ui,
                    "Surface occluded / other",
                    &format!(
                        "{} / {}",
                        self.surface_health.occluded.load(Ordering::Relaxed),
                        self.surface_health.other.load(Ordering::Relaxed)
                    ),
                );
                diagnostic_row(
                    ui,
                    "Device loss",
                    self.device_lost.lock().as_deref().unwrap_or("none"),
                );
                diagnostic_row(
                    ui,
                    "Explicit service gaps",
                    &self.model.explicit_gap_samples.to_string(),
                );
                diagnostic_row(
                    ui,
                    "Inferred UI sequence gaps",
                    &self.model.inferred_gap_samples.to_string(),
                );
                diagnostic_row(
                    ui,
                    "Duplicate samples rejected",
                    &self.model.duplicate_samples.to_string(),
                );
                diagnostic_row(
                    ui,
                    "UI event batches dropped",
                    &self.ui_dropped_batches.load(Ordering::Relaxed).to_string(),
                );
                if let Some(status) = &self.model.status {
                    diagnostic_row(
                        ui,
                        "Service transport lag",
                        &status.transport_lagged_samples.to_string(),
                    );
                    diagnostic_row(
                        ui,
                        "Connected clients",
                        &status.connected_clients.to_string(),
                    );
                    diagnostic_row(
                        ui,
                        "Service uptime",
                        &format!("{:.1} s", status.uptime_millis as f64 / 1_000.0),
                    );
                }
            });
    }

    fn record_benchmark_frame(
        &mut self,
        context: &egui::Context,
        now: Instant,
        frame_interval: Duration,
        ui_cpu_time: Duration,
    ) {
        let Some(benchmark) = &mut self.benchmark else {
            return;
        };
        if benchmark.recording(now) {
            benchmark.frame_intervals.record(frame_interval, 0);
            benchmark.ui_cpu_times.record(ui_cpu_time, 0);
        }
        if now < benchmark.measurement_end || benchmark.submitted {
            return;
        }

        benchmark.submitted = true;
        let viewport = context.input(|input| input.viewport().clone());
        let pixels_per_point = context.pixels_per_point();
        let logical_size = viewport
            .inner_rect
            .map_or_else(|| context.viewport_rect().size(), |rect| rect.size());
        let logical_viewport_points = Some([logical_size.x, logical_size.y]);
        let physical_viewport_pixels = logical_viewport_points.map(|size| {
            [
                (size[0] * pixels_per_point).round().max(0.0) as u32,
                (size[1] * pixels_per_point).round().max(0.0) as u32,
            ]
        });
        let prepare = self
            .renderer
            .as_ref()
            .map(BiofeedbackRenderer::prepare_stats)
            .unwrap_or_default();
        let p99_ms = milliseconds(benchmark.frame_intervals.percentile(0.99));
        let target_hz = self.performance_target_hz.or(self.display_refresh_hz);
        let (target_frame_interval_ms, sample_count_sufficient, target_met) = frame_target_result(
            self.renderer_hardware_accelerated,
            target_hz,
            benchmark.duration,
            benchmark.frame_intervals.len(),
            p99_ms,
        );
        let report = RenderBenchmarkReport {
            schema_version: 2,
            package_version: env!("CARGO_PKG_VERSION"),
            source_revision: option_env!("KASINA_BUILD_REVISION").unwrap_or("unknown"),
            operating_system: std::env::consts::OS,
            architecture: std::env::consts::ARCH,
            adapter: self.renderer_name.clone(),
            backend: self.renderer_backend.clone(),
            device_type: self.renderer_device_type.clone(),
            driver: self.renderer_driver.clone(),
            driver_info: self.renderer_driver_info.clone(),
            hardware_accelerated: self.renderer_hardware_accelerated,
            present_mode: self.present_mode.clone(),
            desired_maximum_frame_latency: self.desired_maximum_frame_latency,
            logical_viewport_points,
            physical_viewport_pixels,
            native_pixels_per_point: viewport.native_pixels_per_point,
            fullscreen: viewport.fullscreen.or(Some(self.fullscreen)),
            stress_instances: self.stress_instances,
            declared_display_refresh_hz: self.display_refresh_hz,
            performance_target_hz: target_hz,
            target_frame_interval_ms,
            sample_count_sufficient,
            target_met,
            warmup_seconds: benchmark.warmup.as_secs_f64(),
            measured_seconds: benchmark.duration.as_secs_f64(),
            measured_frames: benchmark.frame_intervals.len(),
            achieved_frames_per_second: benchmark.frame_intervals.len() as f64
                / benchmark.duration.as_secs_f64(),
            frame_interval_average_ms: milliseconds(benchmark.frame_intervals.average()),
            frame_interval_p95_ms: milliseconds(benchmark.frame_intervals.percentile(0.95)),
            frame_interval_p99_ms: p99_ms,
            ui_cpu_average_ms: milliseconds(benchmark.ui_cpu_times.average()),
            ui_cpu_p95_ms: milliseconds(benchmark.ui_cpu_times.percentile(0.95)),
            ui_cpu_p99_ms: milliseconds(benchmark.ui_cpu_times.percentile(0.99)),
            wgpu_prepare_average_ms: milliseconds(prepare.average()),
            wgpu_prepare_p95_ms: milliseconds(prepare.percentile(0.95)),
            wgpu_prepare_p99_ms: milliseconds(prepare.percentile(0.99)),
            latest_upload_bytes: prepare.uploaded_bytes(),
            surface_outdated_events: self.surface_health.outdated.load(Ordering::Relaxed),
            surface_lost_events: self.surface_health.lost.load(Ordering::Relaxed),
            surface_occluded_events: self.surface_health.occluded.load(Ordering::Relaxed),
            surface_other_events: self.surface_health.other.load(Ordering::Relaxed),
            device_lost: self.device_lost.lock().clone(),
            note: "Treat this as a native-GPU performance result only when renderer identifies the physical adapter and the run used a normal desktop session.",
        };
        let output = benchmark.output.clone();
        self.benchmark_status = Some(format!("Writing benchmark to {}", output.display()));
        benchmark.writer = Some(
            std::thread::Builder::new()
                .name("kasina-benchmark-writer".to_owned())
                .spawn(move || {
                    let encoded = serde_json::to_vec_pretty(&report)
                        .map_err(|error| format!("serialize benchmark report: {error}"))?;
                    fs::write(&output, encoded).map_err(|error| {
                        format!("write benchmark report at {}: {error}", output.display())
                    })
                })
                .expect("the operating system should allow the benchmark writer thread"),
        );
        context.request_repaint_after(Duration::from_millis(10));
    }

    fn poll_benchmark_writer(&mut self, context: &egui::Context) {
        let Some(benchmark) = &mut self.benchmark else {
            return;
        };
        let Some(writer) = benchmark.writer.as_ref() else {
            return;
        };
        if !writer.is_finished() {
            context.request_repaint_after(Duration::from_millis(10));
            return;
        }
        let outcome = benchmark
            .writer
            .take()
            .expect("checked benchmark writer")
            .join()
            .map_err(|_| "benchmark writer panicked".to_owned())
            .and_then(std::convert::identity);
        match outcome {
            Ok(()) => {
                tracing::info!(path = %benchmark.output.display(), "render benchmark written")
            }
            Err(error) => tracing::error!(%error, "render benchmark failed"),
        }
        context.send_viewport_cmd(egui::ViewportCommand::Close);
    }
}

impl eframe::App for KasinaApp {
    fn logic(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();
        self.update_simulation(Instant::now());
        if context.input(|input| input.key_pressed(egui::Key::F11)) {
            self.fullscreen = !self.fullscreen;
            context.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
        }
        let viewport_visible = context.input(|input| input.viewport().visible());
        if let Some(interval) =
            repaint_interval(self.view, viewport_visible, self.settings.simulation_mode)
        {
            context.request_repaint_after(interval);
        }
        self.poll_benchmark_writer(context);
        self.persist_settings_if_due(context, Instant::now());
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ui_started = Instant::now();
        let now = Instant::now();
        let frame_interval = now.duration_since(self.last_frame);
        self.frame_interval_stats.record(frame_interval, 0);
        self.last_frame = now;
        self.top_bar(ui);
        self.navigation(ui);
        if self.view == View::ThoughtStream {
            let model = if self.settings.simulation_mode {
                &self.simulation.model
            } else {
                &self.model
            };
            let session = model
                .service_info
                .as_ref()
                .map_or("simulation", |info| info.instance_id.as_str());
            self.thoughtstream.update(
                model.samples.get(&(StreamKind::SkinResistance as i32)),
                model.latest(StreamKind::ThoughtStreamAdc),
                session,
                now,
                unix_time_ns(),
                &self.settings.thoughtstream,
            );
        } else {
            self.thoughtstream.deactivate(now);
        }
        egui::CentralPanel::default().show(ui, |ui| match self.view {
            View::Service => self.measurement_service(ui),
            View::Dashboard => self.dashboard(ui),
            View::Raw => self.raw_signals(ui),
            View::BreathKasina => self.breath_kasina(ui),
            View::ThoughtStream => {
                if self.thoughtstream.ui(ui, &mut self.settings.thoughtstream) {
                    self.mark_settings_changed(ui.ctx());
                }
            }
            View::Visualizer => self.visualizer(ui),
            View::Diagnostics => self.diagnostics(ui),
            View::Settings => self.settings(ui),
        });
        let ui_cpu_time = ui_started.elapsed();
        self.ui_cpu_stats.record(ui_cpu_time, 0);
        self.record_benchmark_frame(ui.ctx(), now, frame_interval, ui_cpu_time);
        self.check_smoke_test(ui.ctx());
    }
}

impl Drop for KasinaApp {
    fn drop(&mut self) {
        self.cancellation.cancel();
        self.service_host.finish();
        let _detached = self.network_thread.take();
        let final_settings = self
            .settings_notice
            .is_none()
            .then(|| self.settings.clone().sanitized());
        self.settings_writer.finish(final_settings);
    }
}

fn open_recordings_folder(directory: &str) -> Result<()> {
    let path = std::path::Path::new(directory);
    if !path.is_dir() {
        bail!("The recording folder is not available");
    }
    let program = if cfg!(target_os = "windows") {
        "explorer.exe"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(program).arg(path).spawn()?;
    Ok(())
}

fn metric_label(ui: &mut egui::Ui, label: &str, sample: Option<&Sample>, scale: f64) {
    if let Some(sample) = sample {
        ui.label(format!("{label}: {:.1}", sample.value * scale));
    } else {
        ui.label(format!("{label}: —"));
    }
}

fn diagnostic_row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.strong(label);
    ui.label(value);
    ui.end_row();
}

fn draw_signal(
    ui: &mut egui::Ui,
    label: &str,
    samples: Option<&VecDeque<Sample>>,
    color: egui::Color32,
    height: f32,
) {
    ui.horizontal_wrapped(|ui| {
        ui.label(label);
        if let Some(sample) = samples.and_then(|samples| samples.back()) {
            ui.monospace(format!("{:.2} {}", sample.value, sample.unit));
            for (flag, label) in [
                (quality::SOURCE_INVALID, "invalid reading"),
                (quality::PROBE_ERROR, "probe error"),
                (quality::LOW_BATTERY, "low battery"),
                (quality::RECALIBRATED, "recalculated"),
                (quality::STALE, "no new data"),
                (quality::AFTER_GAP, "after gap"),
            ] {
                if sample.quality_flags & flag != 0 {
                    ui.colored_label(egui::Color32::YELLOW, label);
                }
            }
        } else {
            ui.weak("Waiting for samples");
        }
    });
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), height),
        egui::Sense::hover(),
    );
    ui.painter()
        .rect_filled(rect, 5.0, egui::Color32::from_rgb(18, 22, 29));
    let Some(samples) = samples else {
        return;
    };
    if samples.len() < 2 {
        return;
    }
    let visible = samples.iter().rev().take(600).collect::<Vec<_>>();
    let (minimum, maximum) = visible.iter().fold(
        (f64::INFINITY, f64::NEG_INFINITY),
        |(minimum, maximum), sample| (minimum.min(sample.value), maximum.max(sample.value)),
    );
    let span = (maximum - minimum).max(f64::EPSILON);
    let divisor = (visible.len() - 1) as f32;
    let mut previous = None;
    for (index, sample) in visible.iter().rev().enumerate() {
        let x = rect.left() + rect.width() * index as f32 / divisor;
        let normalized = ((sample.value - minimum) / span) as f32;
        let point = egui::pos2(x, rect.bottom() - normalized * rect.height());
        if sample.quality_flags & quality::AFTER_GAP != 0 {
            previous = None;
        }
        if let Some(previous) = previous {
            ui.painter()
                .line_segment([previous, point], egui::Stroke::new(1.5, color));
        }
        previous = Some(point);
    }
}

fn spawn_network_thread(
    endpoint: String,
    token_path: PathBuf,
    sender: SyncSender<ClientEvent>,
    cancellation: CancellationToken,
    dropped_batches: Arc<AtomicU64>,
    repaint: egui::Context,
    commands: tokio::sync::mpsc::UnboundedReceiver<NetworkCommand>,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("kasina-ipc".to_owned())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = sender.try_send(ClientEvent::Connection(format!(
                        "cannot start networking: {error}"
                    )));
                    repaint.request_repaint();
                    return;
                }
            };
            runtime.block_on(network_supervisor(
                endpoint,
                token_path,
                sender,
                cancellation,
                dropped_batches,
                repaint,
                commands,
            ));
        })
        .expect("the operating system should allow the IPC thread")
}

async fn network_supervisor(
    endpoint: String,
    token_path: PathBuf,
    sender: SyncSender<ClientEvent>,
    cancellation: CancellationToken,
    dropped_batches: Arc<AtomicU64>,
    repaint: egui::Context,
    mut commands: tokio::sync::mpsc::UnboundedReceiver<NetworkCommand>,
) {
    let mut cursors = BTreeMap::new();
    let mut service_instance = None;
    let mut retry = Duration::from_millis(250);
    while !cancellation.is_cancelled() {
        send_event(
            &sender,
            ClientEvent::Connection("connecting to acquisition service".to_owned()),
            &repaint,
        );
        let mut session_state = NetworkSessionState {
            cursors: &mut cursors,
            commands: &mut commands,
            service_instance: &mut service_instance,
        };
        let result = connected_session(
            &endpoint,
            &token_path,
            &sender,
            &cancellation,
            &dropped_batches,
            &repaint,
            &mut session_state,
        )
        .await;
        if cancellation.is_cancelled() {
            break;
        }
        send_event(&sender, ClientEvent::Connected(false), &repaint);
        let mut discarded = false;
        while commands.try_recv().is_ok() {
            discarded = true;
        }
        if discarded {
            send_event(
                &sender,
                ClientEvent::ServiceCommandComplete(Some(
                    "The service disconnected; pending controls were cancelled.".to_owned(),
                )),
                &repaint,
            );
        }
        let detail = match result {
            Ok(()) => "service stream ended".to_owned(),
            Err(error) => {
                debug!(%error, "service client reconnecting");
                format!("service unavailable: {error}; retrying")
            }
        };
        send_event(&sender, ClientEvent::Connection(detail), &repaint);
        tokio::select! {
            () = cancellation.cancelled() => break,
            () = tokio::time::sleep(retry) => {},
            Some(_) = commands.recv() => {
                send_event(&sender, ClientEvent::ServiceCommandComplete(Some("The service is offline. Try again after it reconnects.".to_owned())), &repaint);
                send_event(&sender, ClientEvent::RecordingError("The service is offline. Try again after it reconnects.".to_owned()), &repaint);
            }
        }
        retry = (retry * 2).min(Duration::from_secs(5));
    }
}

struct NetworkSessionState<'a> {
    cursors: &'a mut BTreeMap<i32, u64>,
    commands: &'a mut tokio::sync::mpsc::UnboundedReceiver<NetworkCommand>,
    service_instance: &'a mut Option<String>,
}

async fn connected_session(
    endpoint: &str,
    token_path: &PathBuf,
    sender: &SyncSender<ClientEvent>,
    cancellation: &CancellationToken,
    dropped_batches: &AtomicU64,
    repaint: &egui::Context,
    session: &mut NetworkSessionState<'_>,
) -> Result<()> {
    let token = fs::read_to_string(token_path)
        .with_context(|| format!("read service token at {}", token_path.display()))?;
    let token = token.trim();
    if token.is_empty() {
        bail!("service token file is empty");
    }
    let channel = Endpoint::from_shared(endpoint.to_owned())?
        .connect_timeout(Duration::from_secs(2))
        .connect()
        .await
        .context("connect loopback RPC")?;
    let mut client = KasinaClient::new(channel);
    let info = tokio::time::timeout(
        Duration::from_secs(3),
        client.get_service_info(authenticated_request(
            client_hello("kasina-app", env!("CARGO_PKG_VERSION")),
            token,
        )?),
    )
    .await
    .context("The service handshake timed out")??
    .into_inner();
    if info.protocol_major != kasina_protocol::PROTOCOL_MAJOR {
        bail!("The measurement service needs a compatible app version");
    }
    // The UI must receive the instance identity before any history from it. A full
    // queue restarts this handshake without advancing its instance or cursors.
    send_required_event(sender, ClientEvent::ServiceInfo(info.clone()), repaint)?;
    if session.service_instance.as_deref() != Some(info.instance_id.as_str()) {
        session.cursors.clear();
        *session.service_instance = Some(info.instance_id.clone());
    }
    let supports_controls = info.protocol_minor >= 3;

    let streams = all_streams();
    // Establish the live receiver before taking the history snapshot. Samples produced
    // during the snapshot are then present in both paths and de-duplicated by cursor,
    // instead of being lost in a history/subscription race.
    let mut sample_stream = tokio::time::timeout(
        Duration::from_secs(3),
        client.subscribe_samples(authenticated_request(
            SubscribeRequest {
                client: Some(client_hello("kasina-app", env!("CARGO_PKG_VERSION"))),
                streams: streams.clone(),
            },
            token,
        )?),
    )
    .await
    .context("The service sample subscription timed out")??
    .into_inner();
    let history = tokio::time::timeout(
        Duration::from_secs(3),
        client.get_samples_since(authenticated_request(
            SamplesSinceRequest {
                client: Some(client_hello("kasina-app", env!("CARGO_PKG_VERSION"))),
                cursors: streams
                    .iter()
                    .map(|stream| StreamCursor {
                        stream: *stream,
                        after_sequence: session.cursors.get(stream).copied().unwrap_or(0),
                    })
                    .collect(),
            },
            token,
        )?),
    )
    .await
    .context("The service history request timed out")??
    .into_inner();
    publish_batch(history, sender, dropped_batches, repaint, session.cursors)?;
    if let Ok(Ok(recording)) = tokio::time::timeout(
        Duration::from_secs(3),
        client.get_recording_status(authenticated_request(
            client_hello("kasina-app", env!("CARGO_PKG_VERSION")),
            token,
        )?),
    )
    .await
    {
        send_event(
            sender,
            ClientEvent::Recording(recording.into_inner()),
            repaint,
        );
    }
    let mut status_client = client.clone();
    let mut status_tick = tokio::time::interval(Duration::from_millis(500));
    status_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut ports_tick = tokio::time::interval(Duration::from_secs(5));
    let mut control_tasks = tokio::task::JoinSet::new();
    ports_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    send_required_event(sender, ClientEvent::Connected(true), repaint)?;
    send_event(
        sender,
        ClientEvent::Connection("connected; receiving live samples".to_owned()),
        repaint,
    );
    loop {
        tokio::select! {
            () = cancellation.cancelled() => return Ok(()),
            message = sample_stream.message() => {
                let Some(batch) = message? else {
                    bail!("sample stream closed");
                };
                publish_batch(batch, sender, dropped_batches, repaint, session.cursors)?;
            }
            _ = status_tick.tick() => {
                match tokio::time::timeout(Duration::from_secs(3), status_client.get_status(authenticated_request(
                    client_hello("kasina-app", env!("CARGO_PKG_VERSION")),
                    token,
                )?)).await.context("The service stopped responding")? {
                    Ok(status) => send_event(sender, ClientEvent::Status(status.into_inner()), repaint),
                    Err(error) => return Err(error.into()),
                }
            }
            _ = ports_tick.tick(), if supports_controls => {
                if let Ok(ports) = tokio::time::timeout(Duration::from_secs(3), client.list_thought_stream_ports(authenticated_request(client_hello("kasina-app", env!("CARGO_PKG_VERSION")), token)?)).await
                    && let Ok(ports) = ports {
                    send_event(sender, ClientEvent::SerialPorts(ports.into_inner()), repaint);
                }
            }
            Some(command) = session.commands.recv(), if !session.commands.is_closed() => {
                let mut control_client = client.clone();
                let control_sender = sender.clone();
                let control_repaint = repaint.clone();
                let control_token = token.to_owned();
                control_tasks.spawn(async move {
                    handle_network_command(&mut control_client, command, &control_token, &control_sender, &control_repaint).await
                });
            }
            Some(result) = control_tasks.join_next(), if !control_tasks.is_empty() => {
                result.context("A service control task failed")??;
            }
        }
    }
}

fn recording_state_label(state: RecordingState) -> &'static str {
    match state {
        RecordingState::Unspecified => "Waiting for service",
        RecordingState::Unavailable => "Unavailable",
        RecordingState::Idle => "Ready",
        RecordingState::Recording => "Recording",
        RecordingState::Completed => "Completed",
        RecordingState::Interrupted => "Interrupted",
        RecordingState::Error => "Error",
    }
}

async fn handle_network_command(
    client: &mut KasinaClient<tonic::transport::Channel>,
    command: NetworkCommand,
    token: &str,
    sender: &SyncSender<ClientEvent>,
    repaint: &egui::Context,
) -> Result<()> {
    if !matches!(
        &command,
        NetworkCommand::StartRecording { .. } | NetworkCommand::StopRecording
    ) {
        let result = tokio::time::timeout(
            Duration::from_secs(20),
            handle_service_command(client, command, token, sender, repaint),
        )
        .await;
        let notice = match result {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(format!("Could not update measurements: {error}")),
            Err(_) => Some(
                "The service did not respond to that control. Check its connection and try again."
                    .to_owned(),
            ),
        };
        return send_required_event(sender, ClientEvent::ServiceCommandComplete(notice), repaint);
    }
    let result = tokio::time::timeout(Duration::from_secs(20), async {
        match command {
            NetworkCommand::StartRecording { label, notes } => client
                .start_recording(
                    authenticated_request(
                        StartRecordingRequest {
                            client: Some(client_hello("kasina-app", env!("CARGO_PKG_VERSION"))),
                            label,
                            notes,
                        },
                        token,
                    )
                    .expect("validated service token remains valid metadata"),
                )
                .await
                .map(tonic::Response::into_inner),
            NetworkCommand::StopRecording => client
                .stop_recording(
                    authenticated_request(
                        StopRecordingRequest {
                            client: Some(client_hello("kasina-app", env!("CARGO_PKG_VERSION"))),
                        },
                        token,
                    )
                    .expect("validated service token remains valid metadata"),
                )
                .await
                .map(tonic::Response::into_inner),
            _ => unreachable!("service controls handled above"),
        }
    })
    .await;
    let event = match result {
        Ok(Ok(recording)) => ClientEvent::Recording(recording),
        Ok(Err(error)) => {
            ClientEvent::RecordingError(format!("recording command failed: {error}"))
        }
        Err(_) => ClientEvent::RecordingError(
            "The recording response was not received in time. Its state may have changed; check the session status before trying again.".to_owned(),
        ),
    };
    send_required_event(sender, event, repaint)
}

async fn handle_service_command(
    client: &mut KasinaClient<tonic::transport::Channel>,
    command: NetworkCommand,
    token: &str,
    sender: &SyncSender<ClientEvent>,
    repaint: &egui::Context,
) -> Result<()> {
    let hello = || client_hello("kasina-app", env!("CARGO_PKG_VERSION"));
    match command {
        NetworkCommand::SetDeviceEnabled { device_id, enabled } => {
            client
                .set_device_connection(authenticated_request(
                    DeviceCommand {
                        client: Some(hello()),
                        device_id,
                        connect: enabled,
                    },
                    token,
                )?)
                .await?;
        }
        NetworkCommand::ReconnectDevice { device_id } => {
            for connect in [false, true] {
                client
                    .set_device_connection(authenticated_request(
                        DeviceCommand {
                            client: Some(hello()),
                            device_id: device_id.clone(),
                            connect,
                        },
                        token,
                    )?)
                    .await?;
            }
        }
        NetworkCommand::RefreshSerialPorts => {
            let ports = client
                .list_thought_stream_ports(authenticated_request(hello(), token)?)
                .await?
                .into_inner();
            send_event(sender, ClientEvent::SerialPorts(ports), repaint);
        }
        NetworkCommand::SetThoughtStreamPort(port) => {
            let ports = client
                .set_thought_stream_port(authenticated_request(
                    ThoughtStreamPortCommand {
                        client: Some(hello()),
                        port: port.unwrap_or_default(),
                    },
                    token,
                )?)
                .await?
                .into_inner();
            send_event(sender, ClientEvent::SerialPorts(ports), repaint);
        }
        _ => bail!("not a measurement control"),
    }
    Ok(())
}

fn publish_batch(
    mut batch: SampleBatch,
    sender: &SyncSender<ClientEvent>,
    dropped_batches: &AtomicU64,
    repaint: &egui::Context,
    cursors: &mut BTreeMap<i32, u64>,
) -> Result<()> {
    batch
        .samples
        .retain(|sample| sample.sequence > cursors.get(&sample.stream).copied().unwrap_or(0));
    if batch.samples.is_empty() && batch.gaps.is_empty() {
        return Ok(());
    }
    let next_cursors: Vec<_> = batch
        .samples
        .iter()
        .map(|sample| (sample.stream, sample.sequence))
        .collect();
    match sender.try_send(ClientEvent::Samples(batch)) {
        Ok(()) => {
            for (stream, sequence) in next_cursors {
                let cursor = cursors.entry(stream).or_insert(0);
                *cursor = (*cursor).max(sequence);
            }
            repaint.request_repaint();
            Ok(())
        }
        Err(TrySendError::Full(_)) => {
            dropped_batches.fetch_add(1, Ordering::Relaxed);
            bail!("UI event queue reached backpressure limit")
        }
        Err(TrySendError::Disconnected(_)) => bail!("UI event receiver closed"),
    }
}

fn send_event(sender: &SyncSender<ClientEvent>, event: ClientEvent, repaint: &egui::Context) {
    if sender.try_send(event).is_ok() {
        repaint.request_repaint();
    }
}

fn send_required_event(
    sender: &SyncSender<ClientEvent>,
    event: ClientEvent,
    repaint: &egui::Context,
) -> Result<()> {
    match sender.try_send(event) {
        Ok(()) => {
            repaint.request_repaint();
            Ok(())
        }
        Err(TrySendError::Full(_)) => bail!("UI event queue reached backpressure limit"),
        Err(TrySendError::Disconnected(_)) => bail!("UI event receiver closed"),
    }
}

fn authenticated_request<T>(message: T, token: &str) -> Result<Request<T>> {
    let value = MetadataValue::try_from(token).context("token is not valid RPC metadata")?;
    let mut request = Request::new(message);
    request.metadata_mut().insert(AUTH_HEADER, value);
    Ok(request)
}

fn all_streams() -> Vec<i32> {
    [
        StreamKind::HeartRate,
        StreamKind::RrInterval,
        StreamKind::RespirationForce,
        StreamKind::AccelerationX,
        StreamKind::AccelerationY,
        StreamKind::AccelerationZ,
        StreamKind::SkinResistance,
        StreamKind::ThoughtStreamAdc,
    ]
    .map(|stream| stream as i32)
    .to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(stream: StreamKind, sequence: u64) -> Sample {
        Sample {
            stream: stream as i32,
            source_id: "test".to_owned(),
            sequence,
            monotonic_time_ns: sequence,
            wall_time_unix_ns: sequence,
            device_time_ns: None,
            value: sequence as f64,
            unit: "test".to_owned(),
            quality_flags: 0,
        }
    }

    #[test]
    fn model_rejects_duplicates_and_counts_sequence_gaps() {
        let mut model = ClientModel::default();
        model.apply_batch(SampleBatch {
            samples: vec![
                sample(StreamKind::HeartRate, 1),
                sample(StreamKind::HeartRate, 1),
                sample(StreamKind::HeartRate, 3),
            ],
            gaps: Vec::new(),
            service_batch_sequence: 1,
        });
        assert_eq!(model.duplicate_samples, 1);
        assert_eq!(model.inferred_gap_samples, 1);
        assert_eq!(model.last_sequences[&(StreamKind::HeartRate as i32)], 3);
    }

    #[test]
    fn restarted_service_accepts_new_sequences_without_mixing_old_history() {
        let mut model = ClientModel::default();
        let info = |instance: &str| ServiceInfo {
            instance_id: instance.to_owned(),
            ..Default::default()
        };
        model.apply_service_info(info("first"));
        model.apply_batch(SampleBatch {
            samples: vec![sample(StreamKind::HeartRate, 50)],
            ..Default::default()
        });
        model.apply_service_info(info("first"));
        assert_eq!(model.latest(StreamKind::HeartRate).unwrap().sequence, 50);
        model.apply_service_info(info("restarted"));
        model.apply_batch(SampleBatch {
            samples: vec![sample(StreamKind::HeartRate, 1)],
            ..Default::default()
        });
        assert_eq!(model.latest(StreamKind::HeartRate).unwrap().sequence, 1);
        assert_eq!(model.samples[&(StreamKind::HeartRate as i32)].len(), 1);
        assert_eq!(model.duplicate_samples, 0);
    }

    #[test]
    fn full_ui_queue_rejects_required_handshake_and_completion_events() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(2);
        let repaint = egui::Context::default();
        sender.try_send(ClientEvent::Connected(false)).unwrap();
        sender.try_send(ClientEvent::Connected(false)).unwrap();
        let info = ServiceInfo {
            instance_id: "restarted".to_owned(),
            ..Default::default()
        };
        assert!(
            send_required_event(&sender, ClientEvent::ServiceInfo(info.clone()), &repaint).is_err()
        );
        assert!(send_required_event(&sender, ClientEvent::Connected(true), &repaint).is_err());
        for completion in [
            ClientEvent::ServiceCommandComplete(None),
            ClientEvent::Recording(RecordingStatus::default()),
            ClientEvent::RecordingError("response unavailable".to_owned()),
        ] {
            assert!(send_required_event(&sender, completion, &repaint).is_err());
        }
        assert!(matches!(
            receiver.recv().unwrap(),
            ClientEvent::Connected(false)
        ));
        assert!(matches!(
            receiver.recv().unwrap(),
            ClientEvent::Connected(false)
        ));

        // A retried handshake delivers identity before connected state, rather than
        // silently proceeding with the previous instance's UI history.
        send_required_event(&sender, ClientEvent::ServiceInfo(info), &repaint).unwrap();
        send_required_event(&sender, ClientEvent::Connected(true), &repaint).unwrap();
        assert!(matches!(
            receiver.recv().unwrap(),
            ClientEvent::ServiceInfo(info) if info.instance_id == "restarted"
        ));
        assert!(matches!(
            receiver.recv().unwrap(),
            ClientEvent::Connected(true)
        ));
    }

    #[tokio::test]
    async fn full_ui_queue_propagates_command_completion_failure() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        sender.try_send(ClientEvent::Connected(false)).unwrap();
        let repaint = egui::Context::default();
        let mut client =
            KasinaClient::new(Endpoint::from_static("http://127.0.0.1:1").connect_lazy());
        // Invalid metadata fails locally, so this exercises command error delivery
        // without opening a service or depending on a remote response.
        let result = handle_network_command(
            &mut client,
            NetworkCommand::RefreshSerialPorts,
            "invalid\ntoken",
            &sender,
            &repaint,
        )
        .await;
        assert!(result.is_err());
        receiver.recv().unwrap();
        handle_network_command(
            &mut client,
            NetworkCommand::RefreshSerialPorts,
            "invalid\ntoken",
            &sender,
            &repaint,
        )
        .await
        .unwrap();
        assert!(matches!(
            receiver.recv().unwrap(),
            ClientEvent::ServiceCommandComplete(Some(_))
        ));
    }

    #[test]
    fn full_ui_queue_is_visible_and_does_not_advance_recovery_cursor() {
        let (sender, _receiver) = std::sync::mpsc::sync_channel(0);
        let dropped = AtomicU64::new(0);
        let mut cursors = BTreeMap::new();
        let result = publish_batch(
            SampleBatch {
                samples: vec![sample(StreamKind::HeartRate, 7)],
                gaps: Vec::new(),
                service_batch_sequence: 1,
            },
            &sender,
            &dropped,
            &egui::Context::default(),
            &mut cursors,
        );
        assert!(result.is_err());
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
        assert!(cursors.is_empty());
    }

    #[test]
    fn live_overlap_is_removed_after_history_advances_cursor() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let dropped = AtomicU64::new(0);
        let mut cursors = BTreeMap::from([(StreamKind::HeartRate as i32, 5)]);
        publish_batch(
            SampleBatch {
                samples: vec![
                    sample(StreamKind::HeartRate, 4),
                    sample(StreamKind::HeartRate, 5),
                    sample(StreamKind::HeartRate, 6),
                ],
                gaps: Vec::new(),
                service_batch_sequence: 1,
            },
            &sender,
            &dropped,
            &egui::Context::default(),
            &mut cursors,
        )
        .unwrap();
        let ClientEvent::Samples(batch) = receiver.recv().unwrap() else {
            panic!("expected a sample batch");
        };
        assert_eq!(batch.samples.len(), 1);
        assert_eq!(batch.samples[0].sequence, 6);
        assert_eq!(cursors[&(StreamKind::HeartRate as i32)], 6);
    }

    #[test]
    fn repaint_cadence_tracks_animation_simulation_and_visibility() {
        assert_eq!(
            repaint_interval(View::BreathKasina, Some(true), false),
            Some(ANIMATION_INTERVAL)
        );
        assert_eq!(
            repaint_interval(View::Visualizer, Some(true), true),
            Some(ANIMATION_INTERVAL)
        );
        assert_eq!(
            repaint_interval(View::Visualizer, None, false),
            Some(ANIMATION_INTERVAL)
        );
        assert_eq!(repaint_interval(View::Visualizer, Some(false), true), None);
        assert_eq!(repaint_interval(View::Dashboard, Some(true), false), None);
        assert_eq!(
            repaint_interval(View::Dashboard, Some(true), true),
            Some(SIMULATION_SAMPLE_INTERVAL)
        );
    }

    #[test]
    fn in_app_simulation_generates_sequenced_breath_heart_and_rr_streams() {
        let started = Instant::now();
        let mut simulation = SimulationState::new(started);
        simulation.update(started).unwrap();
        simulation
            .update(started + Duration::from_millis(1_000))
            .unwrap();

        let respiration = simulation
            .model
            .samples
            .get(&(StreamKind::RespirationForce as i32))
            .unwrap();
        let heart_rate = simulation
            .model
            .samples
            .get(&(StreamKind::HeartRate as i32))
            .unwrap();
        let rr = simulation
            .model
            .samples
            .get(&(StreamKind::RrInterval as i32))
            .unwrap();
        assert_eq!(respiration.len(), 11);
        assert_eq!(heart_rate.len(), 2);
        assert_eq!(rr.len(), 2);
        assert_eq!(respiration.back().unwrap().sequence, 11);
        assert_eq!(heart_rate.back().unwrap().sequence, 2);
        assert_eq!(
            respiration.back().unwrap().quality_flags,
            quality::SIMULATED
        );
        assert_eq!(respiration.back().unwrap().source_id, "simulated:in-app");
    }

    #[test]
    fn breath_kasina_tracks_rising_and_falling_force_without_fixed_bounds() {
        let mut state = BreathKasinaState::new(Instant::now());
        state.observe(42.0);
        state.observe(44.0);
        let expanded_target = state.target_expansion;
        state.observe(40.0);
        let contracted_target = state.target_expansion;

        assert!(expanded_target > 0.85);
        assert!(contracted_target < 0.15);
        assert!(state.high_force > state.low_force);
        assert_eq!(state.samples_seen, 3);
    }

    #[test]
    fn breath_kasina_smooths_animation_and_ignores_invalid_samples() {
        let mut state = BreathKasinaState::new(Instant::now());
        state.observe(10.0);
        state.observe(f64::NAN);
        state.observe(11.0);
        let before = state.displayed_expansion;
        let after = state.advance(Duration::from_millis(80));

        assert_eq!(state.samples_seen, 2);
        assert!(after > before);
        assert!(after < state.target_expansion);
    }

    #[test]
    fn each_exhale_to_inhale_transition_starts_one_new_breath_generation() {
        let mut state = BreathKasinaState::new(Instant::now());
        state.samples_seen = 12;

        state.update_breath_direction(-0.2);
        state.update_breath_direction(-0.2);
        assert_eq!(state.breath_direction, BreathDirection::Exhaling);
        assert_eq!(state.breath_generation, 0);
        let completed_position = state.breath_history_position();

        state.update_breath_direction(0.2);
        assert_eq!(state.breath_direction, BreathDirection::Exhaling);
        assert_eq!(state.breath_generation, 0);
        state.update_breath_direction(0.2);
        assert_eq!(state.breath_direction, BreathDirection::Inhaling);
        assert_eq!(state.breath_generation, 1);
        assert_eq!(state.breath_history_position(), completed_position);
        state.update_breath_direction(0.2);
        assert_eq!(state.breath_generation, 1);

        state.update_breath_direction(-0.2);
        state.update_breath_direction(-0.2);
        for _ in 0..3 {
            state.advance_breath_layer(Duration::from_millis(250), 0.80);
        }
        let next_completed_position = state.breath_history_position();
        state.update_breath_direction(0.2);
        state.update_breath_direction(0.2);
        assert_eq!(state.breath_generation, 2);
        assert_eq!(state.breath_history_position(), next_completed_position);
    }

    #[test]
    fn breath_kasina_integrates_independent_breath_modulated_layer_speeds() {
        let mut state = BreathKasinaState::new(Instant::now());
        let options = LuminousMandala {
            inner_rotations_per_second: 0.10,
            middle_rotations_per_second: 0.20,
            third_rotations_per_second: 0.30,
            gold_rotations_per_second: 0.40,
            expansion_speed_multiplier: 3.0,
            ..LuminousMandala::default()
        };

        let phases = state.advance_layer_rotations(Duration::from_millis(250), 0.5, &options);
        assert_eq!(phases, [0.05, 0.10, 0.15, 0.20]);
        let phases = state.advance_layer_rotations(Duration::from_secs(5), 0.5, &options);
        assert_eq!(phases, [0.10, 0.20, 0.30, 0.40]);
    }

    #[test]
    fn surface_statuses_choose_recovery_actions_and_are_counted() {
        let health = Arc::new(SurfaceHealth::default());
        let handler = surface_status_handler(Arc::clone(&health));
        assert!(matches!(
            handler(&eframe::wgpu::CurrentSurfaceTexture::Outdated),
            eframe::egui_wgpu::SurfaceErrorAction::Reconfigure
        ));
        assert!(matches!(
            handler(&eframe::wgpu::CurrentSurfaceTexture::Lost),
            eframe::egui_wgpu::SurfaceErrorAction::RecreateSurface
        ));
        assert!(matches!(
            handler(&eframe::wgpu::CurrentSurfaceTexture::Occluded),
            eframe::egui_wgpu::SurfaceErrorAction::SkipFrame
        ));
        assert!(matches!(
            handler(&eframe::wgpu::CurrentSurfaceTexture::Timeout),
            eframe::egui_wgpu::SurfaceErrorAction::SkipFrame
        ));
        assert_eq!(health.outdated.load(Ordering::Relaxed), 1);
        assert_eq!(health.lost.load(Ordering::Relaxed), 1);
        assert_eq!(health.occluded.load(Ordering::Relaxed), 1);
        assert_eq!(health.other.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn benchmark_cli_rejects_non_finite_and_non_positive_durations() {
        for invalid in ["0", "-1", "301", "NaN", "inf"] {
            assert!(
                Args::try_parse_from(["kasina-app", "--render-benchmark-seconds", invalid])
                    .is_err(),
                "duration {invalid} should be rejected"
            );
        }
        let args = Args::try_parse_from([
            "kasina-app",
            "--render-benchmark-seconds",
            "10",
            "--stress-instances",
            "100000",
        ])
        .unwrap();
        assert_eq!(args.render_benchmark_seconds, Some(10.0));
        assert_eq!(args.stress_instances, 100_000);
        assert!(Args::try_parse_from(["kasina-app", "--display-refresh-hz", "1001"]).is_err());
        assert!(Args::try_parse_from(["kasina-app", "--performance-target-hz", "0"]).is_err());
    }

    #[test]
    fn frame_target_requires_hardware_and_enough_samples() {
        let duration = Duration::from_secs(10);
        assert_eq!(
            frame_target_result(false, Some(60.0), duration, 600, 10.0),
            (Some(1_000.0 / 60.0), Some(true), Some(false))
        );
        assert_eq!(
            frame_target_result(true, Some(60.0), duration, 20, 10.0),
            (Some(1_000.0 / 60.0), Some(false), Some(false))
        );
        assert_eq!(
            frame_target_result(true, Some(120.0), duration, 1_200, 8.0),
            (Some(1_000.0 / 120.0), Some(true), Some(true))
        );
        assert_eq!(
            frame_target_result(true, None, duration, 1_200, 8.0),
            (None, None, None)
        );
    }
}
