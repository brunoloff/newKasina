//! Hardware-independent sensor driver interfaces plus simulation and replay drivers.

use std::future::Future;
use std::io::BufRead;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use kasina_domain::{StreamKind, quality};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

/// Broad device class used for selection and status display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeviceKind {
    /// Polar heart sensor.
    Polar,
    /// Vernier Go Direct device.
    GoDirect,
    /// Deterministic development source.
    Simulated,
}

/// Stable device information independent of a BLE backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceDescriptor {
    /// Opaque platform or simulated identifier.
    pub id: String,
    /// User-visible name.
    pub name: String,
    /// Device family.
    pub kind: DeviceKind,
}

/// Machine-readable acquisition state, independent of user-facing diagnostic text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverConnectionState {
    /// The supervisor is discovering or setting up a device.
    Connecting,
    /// Notifications are configured and the source is ready.
    Connected,
    /// A failed session will be attempted again after backoff.
    Reconnecting,
    /// Acquisition stopped without an active connection.
    Disconnected,
}

/// Unsequenced event sent from a driver to the acquisition service.
#[derive(Debug, Clone, PartialEq)]
pub enum DriverEvent {
    /// One scalar measurement. The service owns receive timestamps and sequence numbers.
    Measurement {
        /// Logical stream.
        stream: StreamKind,
        /// Stable source device identifier.
        source_id: String,
        /// Optional timestamp reported by the device.
        device_time_ns: Option<u64>,
        /// Value in the canonical unit of `stream`.
        value: f64,
        /// Quality flags forwarded into the domain sample.
        quality_flags: u32,
    },
    /// Human-readable connection information for diagnostics.
    Status {
        /// Source device identifier.
        source_id: String,
        /// Structured connection state.
        state: DriverConnectionState,
        /// Current detail.
        detail: String,
    },
}

/// Cancellable bounded-channel send used by acquisition drivers.
pub async fn send_driver_event(
    sender: &mpsc::Sender<DriverEvent>,
    cancellation: &CancellationToken,
    event: DriverEvent,
) -> Result<bool> {
    tokio::select! {
        () = cancellation.cancelled() => Ok(false),
        result = sender.send(event) => {
            result?;
            Ok(true)
        }
    }
}

/// Await one fallible driver operation with both cancellation and a hard timeout.
pub async fn cancellable_timeout<T, E, F>(
    cancellation: &CancellationToken,
    timeout: Duration,
    operation: &'static str,
    future: F,
) -> Result<T>
where
    E: std::fmt::Display,
    F: Future<Output = std::result::Result<T, E>>,
{
    tokio::select! {
        () = cancellation.cancelled() => Err(anyhow!("{operation} cancelled")),
        outcome = tokio::time::timeout(timeout, future) => match outcome {
            Err(_) => Err(anyhow!("{operation} timed out after {} ms", timeout.as_millis())),
            Ok(Err(error)) => Err(anyhow!("{operation}: {error}")),
            Ok(Ok(value)) => Ok(value),
        }
    }
}

/// Exponential reconnect delay with deterministic bounded jitter.
#[derive(Debug, Clone)]
pub struct ReconnectBackoff {
    initial_ms: u64,
    current_ms: u64,
    maximum_ms: u64,
    random_state: u64,
}

impl ReconnectBackoff {
    /// Construct a backoff seeded from wall time. The seed is not security-sensitive.
    #[must_use]
    pub fn new(initial: Duration, maximum: Duration) -> Self {
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        Self::with_seed(initial, maximum, seed)
    }

    /// Construct a reproducible backoff for tests.
    #[must_use]
    pub fn with_seed(initial: Duration, maximum: Duration, seed: u64) -> Self {
        let initial_ms = initial.as_millis().clamp(1, u128::from(u64::MAX)) as u64;
        let maximum_ms = maximum
            .as_millis()
            .clamp(u128::from(initial_ms), u128::from(u64::MAX)) as u64;
        Self {
            initial_ms,
            current_ms: initial_ms,
            maximum_ms,
            random_state: seed.max(1),
        }
    }

    /// Reset the exponential component after a successful connection.
    pub fn reset(&mut self) {
        self.current_ms = self.initial_ms;
    }

    /// Return the next delay with up to 20 percent jitter and advance the base delay.
    pub fn next_delay(&mut self) -> Duration {
        self.random_state ^= self.random_state << 13;
        self.random_state ^= self.random_state >> 7;
        self.random_state ^= self.random_state << 17;
        let jitter = self.current_ms / 5;
        let range = jitter.saturating_mul(2).saturating_add(1);
        let signed_offset = (self.random_state % range) as i128 - i128::from(jitter);
        let delayed = (i128::from(self.current_ms) + signed_offset)
            .clamp(1, i128::from(self.maximum_ms)) as u64;
        self.current_ms = self.current_ms.saturating_mul(2).min(self.maximum_ms);
        Duration::from_millis(delayed)
    }
}

/// Long-running sensor producer owned by the service.
#[async_trait]
pub trait SensorDriver: Send {
    /// Describe the source before it begins producing data.
    fn descriptor(&self) -> DeviceDescriptor;

    /// Run until cancelled, disconnected, or a fatal error occurs.
    async fn run(
        self: Box<Self>,
        sender: mpsc::Sender<DriverEvent>,
        cancellation: CancellationToken,
    ) -> Result<()>;
}

/// Deterministic dual-stream development source.
#[derive(Debug, Clone)]
pub struct SimulatedDriver {
    descriptor: DeviceDescriptor,
    sample_period: Duration,
}

impl Default for SimulatedDriver {
    fn default() -> Self {
        Self {
            descriptor: DeviceDescriptor {
                id: "simulated:biofeedback".to_owned(),
                name: "Simulated Polar + respiration belt".to_owned(),
                kind: DeviceKind::Simulated,
            },
            sample_period: Duration::from_millis(100),
        }
    }
}

impl SimulatedDriver {
    /// Construct a simulation with a custom respiration sample period.
    #[must_use]
    pub fn with_period(sample_period: Duration) -> Self {
        Self {
            sample_period,
            ..Self::default()
        }
    }
}

#[async_trait]
impl SensorDriver for SimulatedDriver {
    fn descriptor(&self) -> DeviceDescriptor {
        self.descriptor.clone()
    }

    async fn run(
        self: Box<Self>,
        sender: mpsc::Sender<DriverEvent>,
        cancellation: CancellationToken,
    ) -> Result<()> {
        let source_id = self.descriptor.id.clone();
        if !send_driver_event(
            &sender,
            &cancellation,
            DriverEvent::Status {
                source_id: source_id.clone(),
                state: DriverConnectionState::Connected,
                detail: "connected".to_owned(),
            },
        )
        .await?
        {
            return Ok(());
        }

        let started = Instant::now();
        let mut ticker = tokio::time::interval(self.sample_period);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut tick = 0_u64;

        'simulation: loop {
            tokio::select! {
                () = cancellation.cancelled() => break,
                _ = ticker.tick() => {
                    let seconds = started.elapsed().as_secs_f64();
                    let breath_phase = seconds * std::f64::consts::TAU / 10.0;
                    let respiratory_force = 50.0 + breath_phase.sin() * 22.0
                        + (breath_phase * 3.0).sin() * 1.5;
                    if !send_driver_event(&sender, &cancellation, DriverEvent::Measurement {
                        stream: StreamKind::RespirationForce,
                        source_id: source_id.clone(),
                        device_time_ns: None,
                        value: respiratory_force,
                        quality_flags: quality::SIMULATED,
                    }).await? {
                        break 'simulation;
                    }

                    if tick.is_multiple_of(10) {
                        let bpm = 66.0 + breath_phase.sin() * 5.5;
                        let rr_us = 60_000_000.0 / bpm;
                        for (stream, value) in [
                            (StreamKind::HeartRate, bpm),
                            (StreamKind::RrInterval, rr_us),
                        ] {
                            if !send_driver_event(&sender, &cancellation, DriverEvent::Measurement {
                                stream,
                                source_id: source_id.clone(),
                                device_time_ns: None,
                                value,
                                quality_flags: quality::SIMULATED,
                            }).await? {
                                break 'simulation;
                            }
                        }
                    }
                    tick = tick.saturating_add(1);
                }
            }
        }
        Ok(())
    }
}

/// One portable JSON-lines replay record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayFrame {
    /// Delay from the previous frame.
    pub delay_ms: u64,
    /// Stream to emit.
    pub stream: StreamKind,
    /// Source identifier.
    pub source_id: String,
    /// Optional device timestamp.
    pub device_time_ns: Option<u64>,
    /// Scalar value.
    pub value: f64,
    /// Quality bitfield.
    #[serde(default)]
    pub quality_flags: u32,
}

/// Finite deterministic replay source used by tests and offline development.
#[derive(Debug, Clone)]
pub struct ReplayDriver {
    descriptor: DeviceDescriptor,
    frames: Vec<ReplayFrame>,
}

impl ReplayDriver {
    /// Construct a replay from already decoded frames.
    #[must_use]
    pub fn new(frames: Vec<ReplayFrame>) -> Self {
        Self {
            descriptor: DeviceDescriptor {
                id: "replay:fixture".to_owned(),
                name: "Recorded fixture".to_owned(),
                kind: DeviceKind::Simulated,
            },
            frames,
        }
    }

    /// Decode newline-delimited JSON. Blank lines and `#` comments are ignored.
    pub fn from_json_lines(reader: impl BufRead) -> Result<Self> {
        let mut frames = Vec::new();
        for line in reader.lines() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            frames.push(serde_json::from_str(trimmed)?);
        }
        Ok(Self::new(frames))
    }
}

#[async_trait]
impl SensorDriver for ReplayDriver {
    fn descriptor(&self) -> DeviceDescriptor {
        self.descriptor.clone()
    }

    async fn run(
        self: Box<Self>,
        sender: mpsc::Sender<DriverEvent>,
        cancellation: CancellationToken,
    ) -> Result<()> {
        for frame in self.frames {
            tokio::select! {
                () = cancellation.cancelled() => break,
                () = tokio::time::sleep(Duration::from_millis(frame.delay_ms)) => {
                    if !send_driver_event(&sender, &cancellation, DriverEvent::Measurement {
                        stream: frame.stream,
                        source_id: frame.source_id,
                        device_time_ns: frame.device_time_ns,
                        value: frame.value,
                        quality_flags: frame.quality_flags,
                    }).await? {
                        break;
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn parses_json_lines_fixture() {
        let input = r#"
# synthetic data only
{"delay_ms":0,"stream":"HeartRate","source_id":"test","device_time_ns":null,"value":64.0}
{"delay_ms":1,"stream":"RrInterval","source_id":"test","device_time_ns":1,"value":937500.0,"quality_flags":1}
"#;
        let replay = ReplayDriver::from_json_lines(Cursor::new(input)).unwrap();
        assert_eq!(replay.frames.len(), 2);
        assert_eq!(replay.frames[1].stream, StreamKind::RrInterval);
    }

    #[tokio::test]
    async fn replay_emits_frames_in_order() {
        let frames = vec![
            ReplayFrame {
                delay_ms: 0,
                stream: StreamKind::HeartRate,
                source_id: "test".to_owned(),
                device_time_ns: None,
                value: 60.0,
                quality_flags: 0,
            },
            ReplayFrame {
                delay_ms: 0,
                stream: StreamKind::RrInterval,
                source_id: "test".to_owned(),
                device_time_ns: None,
                value: 1_000_000.0,
                quality_flags: 0,
            },
        ];
        let (sender, mut receiver) = mpsc::channel(4);
        Box::new(ReplayDriver::new(frames))
            .run(sender, CancellationToken::new())
            .await
            .unwrap();
        assert!(matches!(
            receiver.recv().await,
            Some(DriverEvent::Measurement {
                stream: StreamKind::HeartRate,
                ..
            })
        ));
        assert!(matches!(
            receiver.recv().await,
            Some(DriverEvent::Measurement {
                stream: StreamKind::RrInterval,
                ..
            })
        ));
    }

    #[test]
    fn reconnect_backoff_is_jittered_bounded_and_resettable() {
        let mut backoff =
            ReconnectBackoff::with_seed(Duration::from_millis(500), Duration::from_secs(30), 42);
        let delays = (0..10).map(|_| backoff.next_delay()).collect::<Vec<_>>();
        assert!((Duration::from_millis(400)..=Duration::from_millis(600)).contains(&delays[0]));
        assert!(delays.iter().all(|delay| *delay <= Duration::from_secs(30)));
        assert!(delays.windows(2).take(5).all(|pair| pair[1] > pair[0]));

        backoff.reset();
        assert!(
            (Duration::from_millis(400)..=Duration::from_millis(600))
                .contains(&backoff.next_delay())
        );
    }

    #[tokio::test]
    async fn bounded_driver_operations_observe_cancellation_and_timeout() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let cancelled = cancellable_timeout(
            &cancellation,
            Duration::from_secs(1),
            "test operation",
            std::future::pending::<std::result::Result<(), &'static str>>(),
        )
        .await
        .unwrap_err();
        assert!(cancelled.to_string().contains("cancelled"));

        let timeout = cancellable_timeout(
            &CancellationToken::new(),
            Duration::from_millis(1),
            "test operation",
            std::future::pending::<std::result::Result<(), &'static str>>(),
        )
        .await
        .unwrap_err();
        assert!(timeout.to_string().contains("timed out"));
    }
}
