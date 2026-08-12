//! Shared data types and bounded buffers for physiological sample streams.

use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A logically independent physiological data stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum StreamKind {
    /// Heart rate in beats per minute.
    HeartRate,
    /// Time between successive heartbeats in microseconds.
    RrInterval,
    /// Respiration-belt force in device units.
    RespirationForce,
    /// X acceleration.
    AccelerationX,
    /// Y acceleration.
    AccelerationY,
    /// Z acceleration.
    AccelerationZ,
}

impl StreamKind {
    /// All currently defined stream kinds in stable display order.
    pub const ALL: [Self; 6] = [
        Self::HeartRate,
        Self::RrInterval,
        Self::RespirationForce,
        Self::AccelerationX,
        Self::AccelerationY,
        Self::AccelerationZ,
    ];

    /// Canonical unit used on the wire and in recordings.
    #[must_use]
    pub const fn unit(self) -> &'static str {
        match self {
            Self::HeartRate => "bpm",
            Self::RrInterval => "us",
            Self::RespirationForce => "device",
            Self::AccelerationX | Self::AccelerationY | Self::AccelerationZ => "m/s^2",
        }
    }
}

/// Quality flags carried with a sample.
pub mod quality {
    /// The sample was synthesized rather than acquired from hardware.
    pub const SIMULATED: u32 = 1 << 0;
    /// The source reported an invalid or questionable measurement.
    pub const SOURCE_INVALID: u32 = 1 << 1;
    /// A discontinuity preceded this sample.
    pub const AFTER_GAP: u32 = 1 << 2;
}

/// One timestamped, sequenced scalar measurement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    /// Logical stream.
    pub stream: StreamKind,
    /// Stable opaque source identifier.
    pub source_id: String,
    /// Per-stream sequence number, starting at one.
    pub sequence: u64,
    /// Nanoseconds since this service instance started.
    pub monotonic_time_ns: u64,
    /// Nanoseconds since the Unix epoch, captured by the service.
    pub wall_time_unix_ns: u64,
    /// Optional timestamp supplied by the device.
    pub device_time_ns: Option<u64>,
    /// Scalar value in `unit`.
    pub value: f64,
    /// Canonical unit string.
    pub unit: String,
    /// Bit field from [`quality`].
    pub quality_flags: u32,
}

impl Sample {
    /// Construct a sample using the canonical unit for the stream.
    #[must_use]
    pub fn new(
        stream: StreamKind,
        source_id: impl Into<String>,
        sequence: u64,
        monotonic_time_ns: u64,
        wall_time_unix_ns: u64,
        value: f64,
    ) -> Self {
        Self {
            stream,
            source_id: source_id.into(),
            sequence,
            monotonic_time_ns,
            wall_time_unix_ns,
            device_time_ns: None,
            value,
            unit: stream.unit().to_owned(),
            quality_flags: 0,
        }
    }
}

/// Errors that protect sequence and stream invariants.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum BufferError {
    /// A sample for another stream was pushed into this buffer.
    #[error("sample stream {actual:?} does not match buffer stream {expected:?}")]
    WrongStream {
        /// Expected stream.
        expected: StreamKind,
        /// Actual stream.
        actual: StreamKind,
    },
    /// Sequence numbers must increase strictly.
    #[error("sequence {incoming} is not newer than {newest}")]
    NonIncreasingSequence {
        /// Last sequence already stored.
        newest: u64,
        /// Incoming sequence.
        incoming: u64,
    },
    /// Monotonic timestamps may not move backwards.
    #[error("monotonic timestamp {incoming} is older than {newest}")]
    NonMonotonicTime {
        /// Last timestamp already stored.
        newest: u64,
        /// Incoming timestamp.
        incoming: u64,
    },
}

/// Result of a sequence-based snapshot request.
#[derive(Debug, Clone, PartialEq)]
pub struct BufferSnapshot {
    /// Retained samples newer than the requested sequence.
    pub samples: Vec<Sample>,
    /// Whether data between the request cursor and oldest retained sample was lost.
    pub gap: bool,
    /// Oldest sequence still retained, or zero when empty.
    pub oldest_available_sequence: u64,
    /// Newest sequence retained, or zero when empty.
    pub newest_sequence: u64,
    /// Total number evicted from this in-memory buffer.
    pub evicted_samples: u64,
}

/// A time-bounded, sequence-checked buffer for one stream.
#[derive(Debug, Clone)]
pub struct SampleBuffer {
    stream: StreamKind,
    retention_ns: u64,
    samples: VecDeque<Sample>,
    evicted_samples: u64,
}

impl SampleBuffer {
    /// Create an empty buffer.
    #[must_use]
    pub fn new(stream: StreamKind, retention: Duration) -> Self {
        Self {
            stream,
            retention_ns: retention.as_nanos().min(u128::from(u64::MAX)) as u64,
            samples: VecDeque::new(),
            evicted_samples: 0,
        }
    }

    /// Add a sample and evict values older than the configured time horizon.
    pub fn push(&mut self, sample: Sample) -> Result<(), BufferError> {
        if sample.stream != self.stream {
            return Err(BufferError::WrongStream {
                expected: self.stream,
                actual: sample.stream,
            });
        }
        if let Some(newest) = self.samples.back() {
            if sample.sequence <= newest.sequence {
                return Err(BufferError::NonIncreasingSequence {
                    newest: newest.sequence,
                    incoming: sample.sequence,
                });
            }
            if sample.monotonic_time_ns < newest.monotonic_time_ns {
                return Err(BufferError::NonMonotonicTime {
                    newest: newest.monotonic_time_ns,
                    incoming: sample.monotonic_time_ns,
                });
            }
        }

        let newest_time = sample.monotonic_time_ns;
        self.samples.push_back(sample);
        while self.samples.front().is_some_and(|oldest| {
            newest_time.saturating_sub(oldest.monotonic_time_ns) > self.retention_ns
        }) {
            self.samples.pop_front();
            self.evicted_samples = self.evicted_samples.saturating_add(1);
        }
        Ok(())
    }

    /// Return all retained samples whose sequence is newer than `after_sequence`.
    #[must_use]
    pub fn snapshot_after(&self, after_sequence: u64) -> BufferSnapshot {
        let oldest = self.samples.front().map_or(0, |sample| sample.sequence);
        let newest = self.samples.back().map_or(0, |sample| sample.sequence);
        let gap = oldest != 0 && after_sequence.saturating_add(1) < oldest;
        let samples = self
            .samples
            .iter()
            .filter(|sample| sample.sequence > after_sequence)
            .cloned()
            .collect();
        BufferSnapshot {
            samples,
            gap,
            oldest_available_sequence: oldest,
            newest_sequence: newest,
            evicted_samples: self.evicted_samples,
        }
    }

    /// Number of currently retained samples.
    #[must_use]
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// Whether the buffer is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Duration covered by retained samples.
    #[must_use]
    pub fn retained_duration(&self) -> Duration {
        let Some(first) = self.samples.front() else {
            return Duration::ZERO;
        };
        let last = self.samples.back().expect("a non-empty buffer has a back");
        Duration::from_nanos(
            last.monotonic_time_ns
                .saturating_sub(first.monotonic_time_ns),
        )
    }

    /// Newest retained sample.
    #[must_use]
    pub fn newest(&self) -> Option<&Sample> {
        self.samples.back()
    }

    /// Total number evicted due to retention.
    #[must_use]
    pub const fn evicted_samples(&self) -> u64 {
        self.evicted_samples
    }
}

/// Collection of one buffer per stream with a shared retention policy.
#[derive(Debug)]
pub struct ServiceBuffers {
    retention: Duration,
    buffers: BTreeMap<StreamKind, SampleBuffer>,
}

impl ServiceBuffers {
    /// Construct empty service buffers.
    #[must_use]
    pub fn new(retention: Duration) -> Self {
        Self {
            retention,
            buffers: BTreeMap::new(),
        }
    }

    /// Insert a sample into its stream buffer.
    pub fn push(&mut self, sample: Sample) -> Result<(), BufferError> {
        self.buffers
            .entry(sample.stream)
            .or_insert_with(|| SampleBuffer::new(sample.stream, self.retention))
            .push(sample)
    }

    /// Access a stream buffer.
    #[must_use]
    pub fn get(&self, stream: StreamKind) -> Option<&SampleBuffer> {
        self.buffers.get(&stream)
    }

    /// Iterate over populated stream buffers.
    pub fn iter(&self) -> impl Iterator<Item = (StreamKind, &SampleBuffer)> {
        self.buffers.iter().map(|(kind, buffer)| (*kind, buffer))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(sequence: u64, time_ns: u64) -> Sample {
        Sample::new(
            StreamKind::HeartRate,
            "test",
            sequence,
            time_ns,
            time_ns,
            60.0,
        )
    }

    #[test]
    fn evicts_by_time_and_reports_a_cursor_gap() {
        let mut buffer = SampleBuffer::new(StreamKind::HeartRate, Duration::from_secs(2));
        buffer.push(sample(1, 0)).unwrap();
        buffer.push(sample(2, 1_000_000_000)).unwrap();
        buffer.push(sample(3, 3_000_000_000)).unwrap();

        let snapshot = buffer.snapshot_after(0);
        assert_eq!(
            snapshot
                .samples
                .iter()
                .map(|s| s.sequence)
                .collect::<Vec<_>>(),
            [2, 3]
        );
        assert!(snapshot.gap);
        assert_eq!(snapshot.oldest_available_sequence, 2);
        assert_eq!(snapshot.evicted_samples, 1);
    }

    #[test]
    fn snapshot_is_exclusive_and_does_not_duplicate() {
        let mut buffer = SampleBuffer::new(StreamKind::HeartRate, Duration::from_secs(10));
        for sequence in 1..=4 {
            buffer.push(sample(sequence, sequence * 100)).unwrap();
        }
        let snapshot = buffer.snapshot_after(2);
        assert_eq!(
            snapshot
                .samples
                .iter()
                .map(|s| s.sequence)
                .collect::<Vec<_>>(),
            [3, 4]
        );
        assert!(!snapshot.gap);
    }

    #[test]
    fn rejects_non_increasing_sequences_and_time() {
        let mut buffer = SampleBuffer::new(StreamKind::HeartRate, Duration::from_secs(10));
        buffer.push(sample(2, 200)).unwrap();
        assert!(matches!(
            buffer.push(sample(2, 300)),
            Err(BufferError::NonIncreasingSequence { .. })
        ));
        assert!(matches!(
            buffer.push(sample(3, 100)),
            Err(BufferError::NonMonotonicTime { .. })
        ));
    }
}
