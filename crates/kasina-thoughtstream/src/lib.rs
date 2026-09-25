//! ThoughtStream USB serial acquisition, ported from Bruno's pyThoughtstream reader.
//!
//! The device streams eight-byte packets at 19200 baud, 8N1, without host commands.
//! See docs/hardware/thoughtstream-protocol.md for provenance and flag semantics.

use std::collections::VecDeque;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use kasina_devices::{
    DeviceDescriptor, DeviceKind, DriverConnectionState, DriverEvent, ReconnectBackoff,
    SensorDriver, send_driver_event,
};
use kasina_domain::{StreamKind, quality};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use tokio_serial::{DataBits, FlowControl, Parity, SerialPortBuilderExt, StopBits};
use tokio_util::sync::CancellationToken;

const PACKET_TIMEOUT: Duration = Duration::from_secs(10);

/// A checksum-validated packet, retaining the native ADC and status byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Packet {
    /// Big-endian ADC reading.
    pub adc: u16,
    /// Device bits: probe error, low battery, new data, recalculation (bits 0–3).
    pub status: u8,
    /// Bytes were discarded while finding this packet.
    pub after_gap: bool,
}

impl Packet {
    /// Convert using the original Python formula. Undefined/nonphysical values are omitted.
    #[must_use]
    pub fn resistance_ohms(self) -> Option<f64> {
        if self.adc == 0 {
            return None;
        }
        let value = 7_700_010_000.0 / f64::from(self.adc) - 470_000.0;
        (value >= 0.0 && value.is_finite()).then_some(value)
    }

    /// Translate device status into shared, recording-compatible quality bits.
    #[must_use]
    pub fn quality_flags(self) -> u32 {
        let mut flags = 0;
        if self.status & 1 != 0 {
            flags |= quality::PROBE_ERROR | quality::SOURCE_INVALID;
        }
        if self.resistance_ohms().is_none() {
            flags |= quality::SOURCE_INVALID;
        }
        if self.status & 2 != 0 {
            flags |= quality::LOW_BATTERY;
        }
        if self.status & 4 == 0 {
            flags |= quality::STALE;
        }
        if self.status & 8 != 0 {
            flags |= quality::RECALIBRATED;
        }
        if self.after_gap {
            flags |= quality::AFTER_GAP;
        }
        flags
    }
}

/// Incremental decoder with at most eight buffered bytes; handles arbitrary read boundaries.
#[derive(Debug, Default)]
pub struct PacketDecoder {
    bytes: VecDeque<u8>,
    after_gap: bool,
}

impl PacketDecoder {
    /// Feed one serial read, resynchronizing after noise or corrupt packets.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Packet> {
        let mut packets = Vec::new();
        for &byte in bytes {
            self.bytes.push_back(byte);
            loop {
                let header_matches = self.bytes.iter().zip([0xa3, 0x5b, 8]).all(|(a, b)| *a == b);
                if !header_matches {
                    self.bytes.pop_front();
                    self.after_gap = true;
                    continue;
                }
                if self.bytes.len() < 8 {
                    break;
                }
                let checksum: u16 = self.bytes.iter().take(6).map(|b| u16::from(*b)).sum();
                if checksum != u16::from_be_bytes([self.bytes[6], self.bytes[7]]) {
                    self.bytes.pop_front();
                    self.after_gap = true;
                    continue;
                }
                packets.push(Packet {
                    adc: u16::from_be_bytes([self.bytes[3], self.bytes[4]]),
                    status: self.bytes[5],
                    after_gap: std::mem::take(&mut self.after_gap),
                });
                self.bytes.clear();
                break;
            }
        }
        packets
    }
}

/// A serial port shown by the service's native port chooser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SerialPortChoice {
    /// Stable by-id path on Linux when available, otherwise the platform port name.
    pub path: String,
    /// Human-readable device and port description.
    pub label: String,
    /// This port belongs to a USB device.
    pub usb: bool,
    /// Whether this is a likely ThoughtStream adapter to probe automatically.
    pub automatic_candidate: bool,
}

/// Enumerate serial ports without opening them.
pub fn available_serial_ports() -> Result<Vec<SerialPortChoice>> {
    let mut choices = tokio_serial::available_ports()?
        .into_iter()
        .map(|port| {
            let (product, candidate) = match &port.port_type {
                tokio_serial::SerialPortType::UsbPort(info) => {
                    let product = info
                        .product
                        .clone()
                        .unwrap_or_else(|| "USB serial device".to_owned());
                    let named = product
                        .to_ascii_lowercase()
                        .replace(' ', "")
                        .contains("thoughtstream");
                    (product, named || (info.vid == 0x10c4 && info.pid == 0xea60))
                }
                _ => ("Serial port".to_owned(), false),
            };
            SerialPortChoice {
                path: stable_port_path(&port.port_name),
                usb: matches!(port.port_type, tokio_serial::SerialPortType::UsbPort(_)),
                label: format!("{product} — {}", port.port_name),
                automatic_candidate: candidate,
            }
        })
        .collect::<Vec<_>>();
    choices.sort_by(|a, b| a.path.cmp(&b.path));
    choices.dedup_by(|a, b| a.path == b.path);
    Ok(choices)
}

fn stable_port_path(port: &str) -> String {
    #[cfg(target_os = "linux")]
    if let Ok(target) = std::fs::canonicalize(port)
        && let Ok(entries) = std::fs::read_dir("/dev/serial/by-id")
    {
        let mut matches: Vec<_> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| std::fs::canonicalize(path).is_ok_and(|resolved| resolved == target))
            .collect();
        matches.sort();
        if let Some(path) = matches.first() {
            return path.to_string_lossy().into_owned();
        }
    }
    port.to_owned()
}

/// Shared selection that can reconnect just ThoughtStream while the service keeps running.
#[derive(Debug, Clone)]
pub struct ThoughtStreamPortSelection {
    sender: watch::Sender<Option<String>>,
}

impl ThoughtStreamPortSelection {
    /// None means automatic discovery.
    #[must_use]
    pub fn new(port: Option<String>) -> Self {
        let (sender, _) = watch::channel(port);
        Self { sender }
    }

    /// Current explicit port, or None for automatic discovery.
    #[must_use]
    pub fn selected_port(&self) -> Option<String> {
        self.sender.borrow().clone()
    }

    /// Apply a port choice immediately; selecting it again retries the connection.
    pub fn select(&self, port: Option<String>) {
        self.sender.send_replace(port);
    }
}

/// Reconnecting ThoughtStream driver with live port selection.
#[derive(Debug)]
pub struct ThoughtStreamDriver {
    selection: ThoughtStreamPortSelection,
}

impl Default for ThoughtStreamDriver {
    fn default() -> Self {
        Self::new(None)
    }
}

impl ThoughtStreamDriver {
    /// Select a serial path, or automatically probe likely USB adapters.
    #[must_use]
    pub fn new(port: Option<String>) -> Self {
        Self::with_selection(ThoughtStreamPortSelection::new(port))
    }

    /// Attach a control shared with the tray's port chooser.
    #[must_use]
    pub fn with_selection(selection: ThoughtStreamPortSelection) -> Self {
        Self { selection }
    }
}

async fn selected_session(
    selected: Option<String>,
    sender: &mpsc::Sender<DriverEvent>,
    cancellation: &CancellationToken,
    reconnecting: bool,
    backoff: &mut ReconnectBackoff,
) -> Result<()> {
    let explicit = selected.is_some();
    let candidates = match selected {
        Some(port) => vec![port],
        None => available_serial_ports()?
            .into_iter()
            .filter(|port| port.automatic_candidate)
            .map(|port| port.path)
            .collect(),
    };
    if candidates.is_empty() {
        bail!(
            "ThoughtStream not found — connect it or use Choose ThoughtStream port in the tray menu"
        );
    }
    let mut errors = Vec::new();
    for path in candidates {
        if cancellation.is_cancelled() {
            return Ok(());
        }
        if !send_driver_event(
            sender,
            cancellation,
            DriverEvent::Status {
                source_id: format!("thoughtstream:{path}"),
                state: DriverConnectionState::Connecting,
                detail: format!("Checking serial port {path} for ThoughtStream data"),
            },
        )
        .await?
        {
            return Ok(());
        }
        let result = async {
            let mut port = tokio_serial::new(&path, 19_200)
                .data_bits(DataBits::Eight)
                .parity(Parity::None)
                .stop_bits(StopBits::One)
                .flow_control(FlowControl::None)
                .open_native_async()
                .with_context(|| format!("open {path}"))?;
            // A generic CP2102 is only a candidate: read_session must validate a packet
            // before it reports Connected or emits measurements. No commands are sent.
            read_session(
                &mut port,
                &format!("thoughtstream:{path}"),
                sender,
                cancellation,
                if explicit {
                    PACKET_TIMEOUT
                } else {
                    Duration::from_secs(2)
                },
                reconnecting,
                backoff,
            )
            .await
        }
        .await;
        match result {
            Ok(()) => return Ok(()),
            Err(error) => errors.push(format!("{path}: {error:#}")),
        }
    }
    bail!(
        "{} — use Choose ThoughtStream port in the tray menu",
        errors.join("; ")
    )
}

#[async_trait]
impl SensorDriver for ThoughtStreamDriver {
    fn descriptor(&self) -> DeviceDescriptor {
        DeviceDescriptor {
            id: "thoughtstream:usb".to_owned(),
            name: "ThoughtStream USB".to_owned(),
            kind: DeviceKind::ThoughtStream,
        }
    }

    async fn run(
        self: Box<Self>,
        sender: mpsc::Sender<DriverEvent>,
        cancellation: CancellationToken,
    ) -> Result<()> {
        let runtime_id = self.descriptor().id;
        let mut selection = self.selection.sender.subscribe();
        let mut backoff = ReconnectBackoff::new(Duration::from_secs(1), Duration::from_secs(30));
        let mut reconnecting = false;
        while !cancellation.is_cancelled() {
            if !send_driver_event(
                &sender,
                &cancellation,
                DriverEvent::Status {
                    source_id: runtime_id.clone(),
                    state: DriverConnectionState::Connecting,
                    detail: "opening ThoughtStream USB serial port".to_owned(),
                },
            )
            .await?
            {
                break;
            }
            let selected = selection.borrow_and_update().clone();
            let result = tokio::select! {
                () = cancellation.cancelled() => break,
                _ = selection.changed() => None,
                result = selected_session(selected, &sender, &cancellation, reconnecting, &mut backoff) => Some(result),
            };
            let Some(result) = result else {
                reconnecting = true;
                backoff.reset();
                continue;
            };
            if cancellation.is_cancelled() || sender.is_closed() {
                break;
            }
            let detail = match result {
                Ok(()) => "ThoughtStream stream ended".to_owned(),
                Err(error) => format!("{error:#}"),
            };
            if !send_driver_event(
                &sender,
                &cancellation,
                DriverEvent::Status {
                    source_id: runtime_id.clone(),
                    state: DriverConnectionState::Reconnecting,
                    detail,
                },
            )
            .await?
            {
                break;
            }
            reconnecting = true;
            tokio::select! {
                () = cancellation.cancelled() => break,
                () = tokio::time::sleep(backoff.next_delay()) => {},
                _ = selection.changed() => backoff.reset(),
            }
        }
        Ok(())
    }
}

async fn read_session<R: AsyncRead + Unpin>(
    port: &mut R,
    source_id: &str,
    sender: &mpsc::Sender<DriverEvent>,
    cancellation: &CancellationToken,
    timeout: Duration,
    after_reconnect: bool,
    backoff: &mut ReconnectBackoff,
) -> Result<()> {
    let mut decoder = PacketDecoder {
        after_gap: after_reconnect,
        ..PacketDecoder::default()
    };
    let mut bytes = [0; 256];
    let mut deadline = Instant::now() + timeout;
    let mut previous_status = None;
    let mut resistance_gap = after_reconnect;
    loop {
        let size = tokio::select! {
            () = cancellation.cancelled() => return Ok(()),
            result = tokio::time::timeout_at(deadline, port.read(&mut bytes)) =>
                result.context("no valid ThoughtStream packet before timeout")??,
        };
        if size == 0 {
            bail!("ThoughtStream serial port closed");
        }
        for packet in decoder.push(&bytes[..size]) {
            deadline = Instant::now() + PACKET_TIMEOUT;
            let flags = packet.quality_flags();
            let health = flags & !quality::AFTER_GAP;
            if previous_status != Some(health) {
                let detail = format!(
                    "Probe {}; battery {}; {}; {}",
                    if flags & quality::PROBE_ERROR != 0 {
                        "error"
                    } else {
                        "OK"
                    },
                    if flags & quality::LOW_BATTERY != 0 {
                        "low"
                    } else {
                        "OK"
                    },
                    if flags & quality::STALE != 0 {
                        "no new data"
                    } else {
                        "new data"
                    },
                    if packet.resistance_ohms().is_none() {
                        "invalid ADC"
                    } else if flags & quality::RECALIBRATED != 0 {
                        "recalculated"
                    } else {
                        "reading"
                    }
                );
                if !send_driver_event(
                    sender,
                    cancellation,
                    DriverEvent::Status {
                        source_id: source_id.to_owned(),
                        state: DriverConnectionState::Connected,
                        detail,
                    },
                )
                .await?
                {
                    return Ok(());
                }
                previous_status = Some(health);
                backoff.reset();
            }
            let mut measurements =
                vec![(StreamKind::ThoughtStreamAdc, f64::from(packet.adc), flags)];
            if let Some(resistance) = packet.resistance_ohms() {
                let resistance_flags = flags
                    | if resistance_gap {
                        quality::AFTER_GAP
                    } else {
                        0
                    };
                measurements.push((StreamKind::SkinResistance, resistance, resistance_flags));
                resistance_gap = false;
            } else {
                resistance_gap = true;
            }
            for (stream, value, quality_flags) in measurements {
                if !send_driver_event(
                    sender,
                    cancellation,
                    DriverEvent::Measurement {
                        stream,
                        source_id: source_id.to_owned(),
                        device_time_ns: None,
                        value,
                        quality_flags,
                    },
                )
                .await?
                {
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    fn frame(adc: u16, status: u8) -> [u8; 8] {
        let [high, low] = adc.to_be_bytes();
        let mut bytes = [0xa3, 0x5b, 8, high, low, status, 0, 0];
        let checksum: u16 = bytes[..6].iter().map(|b| u16::from(*b)).sum();
        bytes[6..].copy_from_slice(&checksum.to_be_bytes());
        bytes
    }

    #[cfg(unix)]
    #[tokio::test]
    #[cfg_attr(
        target_os = "macos",
        ignore = "macOS pseudo-terminals cannot apply the real serial driver's IOSSIOSPEED baud rate"
    )]
    async fn selecting_a_port_interrupts_backoff_and_switches_an_active_session() {
        use tokio_serial::{SerialPort, SerialStream};

        let (mut first, first_slave) = SerialStream::pair().unwrap();
        let first_path = first_slave.name().unwrap();
        drop(first_slave);
        let (mut second, second_slave) = SerialStream::pair().unwrap();
        let second_path = second_slave.name().unwrap();
        drop(second_slave);
        let selection =
            ThoughtStreamPortSelection::new(Some("/missing-thoughtstream-test-port".to_owned()));
        let (sender, mut receiver) = mpsc::channel(32);
        let cancel = CancellationToken::new();
        let driver = tokio::spawn(
            Box::new(ThoughtStreamDriver::with_selection(selection.clone()))
                .run(sender, cancel.clone()),
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(event) = receiver.recv().await {
                if matches!(
                    event,
                    DriverEvent::Status {
                        state: DriverConnectionState::Reconnecting,
                        ..
                    }
                ) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        selection.select(Some(first_path));
        receive_serial_adc(&mut first, &mut receiver, 10_000).await;
        selection.select(Some(second_path.clone()));
        let event = receive_serial_adc(&mut second, &mut receiver, 12_000).await;
        let DriverEvent::Measurement {
            source_id,
            quality_flags,
            ..
        } = event
        else {
            unreachable!()
        };
        assert_eq!(source_id, format!("thoughtstream:{second_path}"));
        assert_ne!(quality_flags & quality::AFTER_GAP, 0);
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(1), driver)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[cfg(unix)]
    async fn receive_serial_adc(
        port: &mut tokio_serial::SerialStream,
        receiver: &mut mpsc::Receiver<DriverEvent>,
        adc: u16,
    ) -> DriverEvent {
        tokio::time::timeout(Duration::from_secs(2), async {
            let mut tick = tokio::time::interval(Duration::from_millis(20));
            loop {
                tokio::select! {
                    _ = tick.tick() => port.write_all(&frame(adc, 4)).await.unwrap(),
                    event = receiver.recv() => {
                        let event = event.expect("driver stopped unexpectedly");
                        if matches!(event, DriverEvent::Measurement { stream: StreamKind::ThoughtStreamAdc, value, .. } if value == f64::from(adc)) {
                            return event;
                        }
                    }
                }
            }
        }).await.expect("selected serial port should start promptly")
    }

    #[test]
    fn python_reference_packet_and_conversion() {
        let packets = PacketDecoder::default().push(&[0xa3, 0x5b, 8, 0x27, 0x10, 4, 1, 0x41]);
        assert_eq!(
            packets,
            [Packet {
                adc: 10_000,
                status: 4,
                after_gap: false
            }]
        );
        assert!((packets[0].resistance_ohms().unwrap() - 300_001.0).abs() < 1e-9);
        assert_eq!(packets[0].quality_flags(), 0);
    }

    #[test]
    fn all_read_splits_and_coalesced_packets() {
        let bytes = [frame(10_000, 4), frame(12_000, 6)].concat();
        for split in 0..=bytes.len() {
            let mut decoder = PacketDecoder::default();
            let mut packets = decoder.push(&bytes[..split]);
            packets.extend(decoder.push(&bytes[split..]));
            assert_eq!(packets.len(), 2);
            assert_eq!(packets[0].adc, 10_000);
            assert_eq!(packets[1].adc, 12_000);
            assert!(!packets.iter().any(|p| p.after_gap));
        }
    }

    #[test]
    fn resynchronizes_after_bad_checksum_truncation_and_overlapping_header() {
        let mut bad = frame(10_000, 4);
        bad[7] ^= 1;
        let bytes = [
            &[0, 0xa3][..],
            &bad,
            &frame(100, 4)[..4],
            &frame(12_000, 4),
            &frame(11_000, 4),
        ]
        .concat();
        let mut decoder = PacketDecoder::default();
        let packets: Vec<_> = bytes.iter().flat_map(|b| decoder.push(&[*b])).collect();
        assert_eq!(packets.len(), 2);
        assert_eq!(packets[0].adc, 12_000);
        assert!(packets[0].after_gap);
        assert!(!packets[1].after_gap);
        assert!(decoder.bytes.len() < 8);
    }

    #[test]
    fn flags_and_invalid_adc_are_preserved_without_infinity() {
        let packet = Packet {
            adc: 0,
            status: 11,
            after_gap: true,
        };
        assert!(packet.resistance_ohms().is_none());
        assert_eq!(
            packet.quality_flags(),
            quality::SOURCE_INVALID
                | quality::PROBE_ERROR
                | quality::LOW_BATTERY
                | quality::STALE
                | quality::RECALIBRATED
                | quality::AFTER_GAP
        );
        assert!(
            Packet {
                adc: u16::MAX,
                ..packet
            }
            .resistance_ohms()
            .is_none()
        );
    }

    #[tokio::test]
    async fn session_emits_flags_and_recovers_resistance_after_zero_adc() {
        let (mut writer, mut reader) = tokio::io::duplex(64);
        let (sender, mut receiver) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        writer
            .write_all(&[frame(0, 4), frame(10_000, 15)].concat())
            .await
            .unwrap();
        drop(writer);
        let mut backoff = ReconnectBackoff::new(Duration::from_millis(1), Duration::from_secs(1));
        assert!(
            read_session(
                &mut reader,
                "thoughtstream:test",
                &sender,
                &cancel,
                Duration::from_secs(1),
                false,
                &mut backoff
            )
            .await
            .is_err()
        );
        drop(sender);
        let mut samples = Vec::new();
        while let Some(event) = receiver.recv().await {
            if let DriverEvent::Measurement {
                stream,
                value,
                quality_flags,
                ..
            } = event
            {
                samples.push((stream, value, quality_flags));
            }
        }
        assert_eq!(samples.len(), 3);
        assert_eq!(
            samples[0],
            (StreamKind::ThoughtStreamAdc, 0.0, quality::SOURCE_INVALID)
        );
        assert_eq!(samples[2].0, StreamKind::SkinResistance);
        assert_eq!(samples[2].1, 300_001.0);
        assert_eq!(
            samples[2].2,
            quality::PROBE_ERROR
                | quality::SOURCE_INVALID
                | quality::LOW_BATTERY
                | quality::RECALIBRATED
                | quality::AFTER_GAP
        );
    }

    #[tokio::test]
    async fn idle_port_times_out_and_cancellation_unblocks_backpressure() {
        let (_writer, mut reader) = tokio::io::duplex(64);
        let (sender, _receiver) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let mut backoff = ReconnectBackoff::new(Duration::from_millis(1), Duration::from_secs(1));
        let error = read_session(
            &mut reader,
            "test",
            &sender,
            &cancel,
            Duration::from_millis(10),
            false,
            &mut backoff,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("no valid ThoughtStream packet"));
        let (mut writer, mut reader) = tokio::io::duplex(64);
        writer.write_all(&frame(10_000, 4)).await.unwrap();
        let stop = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            stop.cancel();
        });
        tokio::time::timeout(
            Duration::from_secs(1),
            read_session(
                &mut reader,
                "test",
                &sender,
                &cancel,
                Duration::from_secs(10),
                false,
                &mut backoff,
            ),
        )
        .await
        .unwrap()
        .unwrap();
    }
}
