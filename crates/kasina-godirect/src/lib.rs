//! Pure Go Direct packet codec and reconnecting `btleplug` respiration driver.
//!
//! The codec implements the wire subset needed by the respiration belt. Hardware
//! discovery and channel selection remain deliberately separate so captured fixtures
//! can validate the protocol without a Bluetooth adapter.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use btleplug::api::{
    Central, Characteristic, Manager as _, Peripheral as _, ScanFilter, ValueNotification,
    WriteType,
};
use btleplug::platform::{Manager, Peripheral};
use futures::{Stream, StreamExt};
use kasina_devices::{
    DeviceDescriptor, DeviceKind, DriverConnectionState, DriverEvent, ReconnectBackoff,
    SensorDriver, cancellable_timeout, send_driver_event,
};
use kasina_domain::{StreamKind, quality};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

const BLE_OPERATION_TIMEOUT: Duration = Duration::from_secs(30);
const NOTIFICATION_TIMEOUT_MINIMUM: Duration = Duration::from_secs(5);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(3);

/// Official Go Direct BLE service UUID.
pub const SERVICE_UUID: uuid::Uuid =
    uuid::Uuid::from_u128(0xd917_14ef_28b9_4f91_ba16_f0d9_a604_f112);
/// Go Direct command characteristic UUID.
pub const COMMAND_UUID: uuid::Uuid =
    uuid::Uuid::from_u128(0xf4bf_14a6_c7d5_4b6d_8aa8_df1a_7c83_adcb);
/// Go Direct response/notification characteristic UUID.
pub const RESPONSE_UUID: uuid::Uuid =
    uuid::Uuid::from_u128(0xb41e_6675_a329_40e0_aa01_44d2_f444_babe);

const PACKET_HEADER: u8 = 0x58;
const MEASUREMENT_PACKET: u8 = 0x20;
const INIT: &[u8] = &[
    0x1a, 0xa5, 0x4a, 0x06, 0x49, 0x07, 0x48, 0x08, 0x47, 0x09, 0x46, 0x0a, 0x45, 0x0b, 0x44, 0x0c,
    0x43, 0x0d, 0x42, 0x0e, 0x41,
];
const SET_MEASUREMENT_PERIOD: &[u8] = &[0x1b, 0xff, 0, 0, 0, 0, 0, 0, 0, 0, 0];
const START_MEASUREMENTS: &[u8] = &[0x18, 0xff, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
const STOP_MEASUREMENTS: &[u8] = &[0x19, 0xff, 0, 0xff, 0xff, 0xff, 0xff];
const GET_STATUS: &[u8] = &[0x10];
const GET_SENSOR_INFO: &[u8] = &[0x50, 0];
const GET_AVAILABLE_SENSORS: &[u8] = &[0x51];
const GET_DEVICE_INFO: &[u8] = &[0x55];
const GET_DEFAULT_SENSORS: &[u8] = &[0x56];
const DISCONNECT: &[u8] = &[0x54];

/// Stateful command encoder; Go Direct echoes the descending rolling counter.
#[derive(Debug, Clone)]
pub struct CommandEncoder {
    rolling_counter: u8,
}

impl Default for CommandEncoder {
    fn default() -> Self {
        Self {
            rolling_counter: 0xff,
        }
    }
}

impl CommandEncoder {
    /// Wrap one Go Direct subcommand in its length/counter/checksum header.
    pub fn encode(&mut self, subcommand: &[u8]) -> Result<Vec<u8>, ProtocolError> {
        let length = 4_usize
            .checked_add(subcommand.len())
            .ok_or(ProtocolError::CommandTooLong(subcommand.len()))?;
        let length =
            u8::try_from(length).map_err(|_| ProtocolError::CommandTooLong(subcommand.len()))?;
        self.rolling_counter = self.rolling_counter.wrapping_sub(1);
        let mut packet = Vec::with_capacity(4 + subcommand.len());
        packet.extend_from_slice(&[PACKET_HEADER, 0, self.rolling_counter, 0]);
        packet.extend_from_slice(subcommand);
        packet[1] = length;
        packet[3] = packet_checksum(&packet);
        Ok(packet)
    }

    /// Initialize a newly connected Go Direct session.
    pub fn initialize(&mut self) -> Result<Vec<u8>, ProtocolError> {
        self.encode(INIT)
    }

    /// Set the measurement period in milliseconds (the wire uses microseconds).
    pub fn set_measurement_period(&mut self, period_ms: u32) -> Result<Vec<u8>, ProtocolError> {
        let mut command = SET_MEASUREMENT_PERIOD.to_vec();
        command[3..7].copy_from_slice(&period_ms.saturating_mul(1_000).to_le_bytes());
        self.encode(&command)
    }

    /// Start a selected channel mask.
    pub fn start_measurements(&mut self, channel_mask: u32) -> Result<Vec<u8>, ProtocolError> {
        let mut command = START_MEASUREMENTS.to_vec();
        command[3..7].copy_from_slice(&channel_mask.to_le_bytes());
        self.encode(&command)
    }

    /// Stop all measurements.
    pub fn stop_measurements(&mut self) -> Result<Vec<u8>, ProtocolError> {
        self.encode(STOP_MEASUREMENTS)
    }

    /// Query firmware, battery, and charging state.
    pub fn get_status(&mut self) -> Result<Vec<u8>, ProtocolError> {
        self.encode(GET_STATUS)
    }

    /// Query order code, serial number, and device name.
    pub fn get_device_info(&mut self) -> Result<Vec<u8>, ProtocolError> {
        self.encode(GET_DEVICE_INFO)
    }

    /// Query the default channel mask.
    pub fn get_default_sensors(&mut self) -> Result<Vec<u8>, ProtocolError> {
        self.encode(GET_DEFAULT_SENSORS)
    }

    /// Query the available channel mask.
    pub fn get_available_sensors(&mut self) -> Result<Vec<u8>, ProtocolError> {
        self.encode(GET_AVAILABLE_SENSORS)
    }

    /// Query metadata for one channel.
    pub fn get_sensor_info(&mut self, channel: u8) -> Result<Vec<u8>, ProtocolError> {
        let mut command = GET_SENSOR_INFO.to_vec();
        command[1] = channel.min(31);
        self.encode(&command)
    }

    /// Ask the device to end its protocol session before transport disconnect.
    pub fn disconnect(&mut self) -> Result<Vec<u8>, ProtocolError> {
        self.encode(DISCONNECT)
    }
}

fn packet_checksum(packet: &[u8]) -> u8 {
    packet
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != 3)
        .fold(0_u8, |sum, (_, value)| sum.wrapping_add(*value))
}

/// A scalar measurement tied to a Go Direct channel number.
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelMeasurement {
    /// Channel bit number reported by sensor metadata.
    pub channel: u8,
    /// Raw floating-point or integer value converted to `f64`.
    pub value: f64,
}

/// Identity fields returned by the Go Direct information command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceIdentity {
    /// Vernier product/order code.
    pub order_code: String,
    /// Device serial number.
    pub serial_number: String,
    /// User-visible device name.
    pub name: String,
}

/// Status fields used for acquisition diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceStatus {
    /// Main firmware major/minor/build version.
    pub main_firmware: String,
    /// BLE firmware major/minor/build version.
    pub ble_firmware: String,
    /// Battery percentage as reported by the device.
    pub battery_percent: u8,
    /// Raw charging-state byte.
    pub charging_state: u8,
}

/// Metadata for one Go Direct measurement channel.
#[derive(Debug, Clone, PartialEq)]
pub struct SensorInfo {
    /// Channel number used in masks and measurement packets.
    pub channel: u8,
    /// Vernier sensor identifier.
    pub sensor_id: u32,
    /// Numeric measurement representation.
    pub numeric_type: u8,
    /// Periodic/aperiodic sampling mode.
    pub sampling_mode: u8,
    /// Human-readable sensor description.
    pub description: String,
    /// Unit string supplied by the sensor.
    pub unit: String,
    /// Measurement uncertainty.
    pub uncertainty: f64,
    /// Minimum measurement value.
    pub minimum: f64,
    /// Maximum measurement value.
    pub maximum: f64,
    /// Minimum supported period in microseconds.
    pub minimum_period_us: u32,
    /// Maximum supported period in microseconds.
    pub maximum_period_us: u64,
    /// Typical period in microseconds.
    pub typical_period_us: u32,
    /// Period granularity in microseconds.
    pub period_granularity_us: u32,
    /// Channels that may not be active simultaneously.
    pub mutual_exclusion_mask: u32,
}

/// Packet parsing failure.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProtocolError {
    /// A command cannot fit in the protocol's one-byte packet length.
    #[error("Go Direct subcommand with {0} bytes exceeds the packet limit")]
    CommandTooLong(usize),
    /// Framing is incomplete or inconsistent.
    #[error("truncated Go Direct packet")]
    Truncated,
    /// A decoded packet must contain exactly its declared number of bytes.
    #[error("Go Direct packet declares {declared} bytes but contains {actual}")]
    LengthMismatch {
        /// Header length.
        declared: usize,
        /// Supplied packet length.
        actual: usize,
    },
    /// This is a command response rather than a measurement.
    #[error("packet is not a measurement notification")]
    NotMeasurement,
    /// Measurement kind is known but carries timing/drop metadata, not values.
    #[error("measurement metadata packet {0:#04x} has no scalar values")]
    Metadata(u8),
    /// Measurement kind is unknown.
    #[error("unknown Go Direct measurement type {0:#04x}")]
    UnknownMeasurementType(u8),
    /// The packet has values for channels whose metadata has not been loaded.
    #[error("measurement value count does not fit packet")]
    ValueCount,
}

fn read_u16(packet: &[u8], offset: usize) -> Result<u16, ProtocolError> {
    let bytes = packet
        .get(offset..offset + 2)
        .ok_or(ProtocolError::Truncated)?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn read_u32(packet: &[u8], offset: usize) -> Result<u32, ProtocolError> {
    let bytes = packet
        .get(offset..offset + 4)
        .ok_or(ProtocolError::Truncated)?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_u64(packet: &[u8], offset: usize) -> Result<u64, ProtocolError> {
    let bytes = packet
        .get(offset..offset + 8)
        .ok_or(ProtocolError::Truncated)?;
    Ok(u64::from_le_bytes(
        bytes.try_into().expect("slice is 8 bytes"),
    ))
}

fn read_f64(packet: &[u8], offset: usize) -> Result<f64, ProtocolError> {
    Ok(f64::from_bits(read_u64(packet, offset)?))
}

fn wire_string(bytes: &[u8]) -> String {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_owned()
}

/// Parse firmware/battery state returned by command `0x10`.
pub fn parse_device_status(payload: &[u8]) -> Result<DeviceStatus, ProtocolError> {
    let bytes = payload.get(..12).ok_or(ProtocolError::Truncated)?;
    Ok(DeviceStatus {
        main_firmware: format!("{}.{}.{}", bytes[2], bytes[3], read_u16(bytes, 4)?),
        ble_firmware: format!("{}.{}.{}", bytes[6], bytes[7], read_u16(bytes, 8)?),
        battery_percent: bytes[10],
        charging_state: bytes[11],
    })
}

/// Parse identity returned by command `0x55`.
pub fn parse_device_identity(payload: &[u8]) -> Result<DeviceIdentity, ProtocolError> {
    let bytes = payload.get(..64).ok_or(ProtocolError::Truncated)?;
    Ok(DeviceIdentity {
        order_code: wire_string(&bytes[0..16]),
        serial_number: wire_string(&bytes[16..32]),
        name: wire_string(&bytes[32..64]),
    })
}

/// Parse a little-endian channel mask returned by `0x51` or `0x56`.
pub fn parse_sensor_mask(payload: &[u8]) -> Result<u32, ProtocolError> {
    read_u32(payload, 0)
}

/// Parse the 148-byte channel metadata payload returned by command `0x50`.
pub fn parse_sensor_info(payload: &[u8]) -> Result<SensorInfo, ProtocolError> {
    let bytes = payload.get(..148).ok_or(ProtocolError::Truncated)?;
    Ok(SensorInfo {
        channel: bytes[0],
        sensor_id: read_u32(bytes, 2)?,
        numeric_type: bytes[6],
        sampling_mode: bytes[7],
        description: wire_string(&bytes[8..68]),
        unit: wire_string(&bytes[68..100]),
        uncertainty: read_f64(bytes, 100)?,
        minimum: read_f64(bytes, 108)?,
        maximum: read_f64(bytes, 116)?,
        minimum_period_us: read_u32(bytes, 124)?,
        maximum_period_us: read_u64(bytes, 128)?,
        typical_period_us: read_u32(bytes, 136)?,
        period_granularity_us: read_u32(bytes, 140)?,
        mutual_exclusion_mask: read_u32(bytes, 144)?,
    })
}

fn channels_from_mask(mask: u32) -> Vec<u8> {
    (0_u8..32)
        .filter(|channel| mask & (1_u32 << channel) != 0)
        .collect()
}

/// Parse one fully reassembled Go Direct measurement packet.
pub fn parse_measurements(packet: &[u8]) -> Result<Vec<ChannelMeasurement>, ProtocolError> {
    if packet.len() < 5 {
        return Err(ProtocolError::Truncated);
    }
    if packet[0] != MEASUREMENT_PACKET {
        return Err(ProtocolError::NotMeasurement);
    }
    let declared_length = usize::from(packet[1]);
    if declared_length > packet.len() {
        return Err(ProtocolError::Truncated);
    }
    if declared_length != packet.len() {
        return Err(ProtocolError::LengthMismatch {
            declared: declared_length,
            actual: packet.len(),
        });
    }

    let measurement_type = packet[4];
    let (channels, count, mut offset, is_float): (Vec<u8>, usize, usize, bool) =
        match measurement_type {
            0x06 => (
                channels_from_mask(u32::from(read_u16(packet, 5)?)),
                usize::from(*packet.get(7).ok_or(ProtocolError::Truncated)?),
                9,
                true,
            ),
            0x07 => (
                channels_from_mask(read_u32(packet, 5)?),
                usize::from(*packet.get(9).ok_or(ProtocolError::Truncated)?),
                11,
                true,
            ),
            0x08 | 0x0a => (
                vec![*packet.get(6).ok_or(ProtocolError::Truncated)?],
                usize::from(*packet.get(7).ok_or(ProtocolError::Truncated)?),
                8,
                true,
            ),
            0x09 | 0x0b => (
                vec![*packet.get(6).ok_or(ProtocolError::Truncated)?],
                usize::from(*packet.get(7).ok_or(ProtocolError::Truncated)?),
                8,
                false,
            ),
            0x0c..=0x0e => return Err(ProtocolError::Metadata(measurement_type)),
            other => return Err(ProtocolError::UnknownMeasurementType(other)),
        };

    let expected = count
        .checked_mul(channels.len())
        .and_then(|values| values.checked_mul(4))
        .and_then(|bytes| offset.checked_add(bytes))
        .ok_or(ProtocolError::ValueCount)?;
    if expected != declared_length || expected > packet.len() {
        return Err(ProtocolError::ValueCount);
    }

    let mut measurements = Vec::with_capacity(count * channels.len());
    for _ in 0..count {
        for &channel in &channels {
            let bytes = packet
                .get(offset..offset + 4)
                .ok_or(ProtocolError::ValueCount)?;
            let bytes = [bytes[0], bytes[1], bytes[2], bytes[3]];
            let value = if is_float {
                f64::from(f32::from_le_bytes(bytes))
            } else {
                f64::from(i32::from_le_bytes(bytes))
            };
            measurements.push(ChannelMeasurement { channel, value });
            offset += 4;
        }
    }
    Ok(measurements)
}

/// Reassemble notifications split at the BLE 20-byte payload boundary.
#[derive(Debug, Default)]
pub struct PacketAssembler {
    buffer: Vec<u8>,
}

impl PacketAssembler {
    /// Append a BLE notification and return a complete packet when its declared length arrives.
    pub fn push(&mut self, fragment: &[u8]) -> Result<Option<Vec<u8>>, ProtocolError> {
        self.buffer.extend_from_slice(fragment);
        self.take_ready()
    }

    /// Return another complete buffered packet without appending a notification.
    pub fn take_ready(&mut self) -> Result<Option<Vec<u8>>, ProtocolError> {
        let Some(length) = self.buffer.get(1).copied().map(usize::from) else {
            return Ok(None);
        };
        if length < 2 {
            self.buffer.clear();
            return Err(ProtocolError::Truncated);
        }
        if self.buffer.len() < length {
            return Ok(None);
        }
        let packet: Vec<_> = self.buffer.drain(..length).collect();
        Ok(Some(packet))
    }
}

/// Go Direct respiration-belt acquisition configuration.
#[derive(Debug, Clone)]
pub struct GoDirectDriver {
    target_id: Option<String>,
    channel: u8,
    period: Duration,
    scan_duration: Duration,
}

impl GoDirectDriver {
    /// Search for any advertised Go Direct device and use pyKasina's channel 1 at 10 Hz.
    #[must_use]
    pub fn respiration_belt() -> Self {
        Self {
            target_id: None,
            channel: 1,
            period: Duration::from_millis(100),
            scan_duration: Duration::from_secs(5),
        }
    }

    /// Prefer a saved platform peripheral identifier.
    #[must_use]
    pub fn with_target_id(target_id: impl Into<String>) -> Self {
        Self {
            target_id: Some(target_id.into()),
            ..Self::respiration_belt()
        }
    }

    /// Override the selected channel and measurement period for protocol investigation.
    #[must_use]
    pub fn with_measurement(mut self, channel: u8, period: Duration) -> Self {
        self.channel = channel.min(31);
        self.period = period.max(Duration::from_millis(10));
        self
    }

    async fn find_peripheral(&self, cancellation: &CancellationToken) -> Result<Peripheral> {
        let manager = cancellable_timeout(
            cancellation,
            BLE_OPERATION_TIMEOUT,
            "create Bluetooth manager",
            Manager::new(),
        )
        .await?;
        let adapters = cancellable_timeout(
            cancellation,
            BLE_OPERATION_TIMEOUT,
            "enumerate Bluetooth adapters",
            manager.adapters(),
        )
        .await?;
        if adapters.is_empty() {
            bail!("no Bluetooth adapter is available");
        }

        let mut scanning = Vec::new();
        for adapter in &adapters {
            if cancellation.is_cancelled() {
                break;
            }
            match cancellable_timeout(
                cancellation,
                BLE_OPERATION_TIMEOUT,
                "start Go Direct scan",
                adapter.start_scan(ScanFilter::default()),
            )
            .await
            {
                Ok(()) => scanning.push(adapter),
                Err(_) if cancellation.is_cancelled() => break,
                Err(error) => warn!(%error, "could not start Go Direct scan on an adapter"),
            }
        }
        if cancellation.is_cancelled() {
            for adapter in &scanning {
                let _ = tokio::time::timeout(CLEANUP_TIMEOUT, adapter.stop_scan()).await;
            }
            bail!("Go Direct scan cancelled");
        }
        if scanning.is_empty() {
            bail!("no Bluetooth adapter could start a Go Direct scan");
        }
        let cancelled = tokio::select! {
            () = cancellation.cancelled() => true,
            () = tokio::time::sleep(self.scan_duration) => false,
        };
        for adapter in &scanning {
            match tokio::time::timeout(CLEANUP_TIMEOUT, adapter.stop_scan()).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => warn!(%error, "could not stop Go Direct scan"),
                Err(_) => warn!("stopping Go Direct scan timed out"),
            }
        }
        if cancelled {
            bail!("Go Direct scan cancelled");
        }

        let mut fallback = None;
        for adapter in &adapters {
            let peripherals = cancellable_timeout(
                cancellation,
                BLE_OPERATION_TIMEOUT,
                "list Go Direct peripherals",
                adapter.peripherals(),
            )
            .await?;
            for peripheral in peripherals {
                let id = peripheral.id().to_string();
                let properties = match cancellable_timeout(
                    cancellation,
                    BLE_OPERATION_TIMEOUT,
                    "read Go Direct advertisement properties",
                    peripheral.properties(),
                )
                .await
                {
                    Ok(properties) => properties,
                    Err(error) => {
                        warn!(%error, %id, "could not read Go Direct advertisement properties");
                        continue;
                    }
                };
                if self.target_id.as_ref().is_some_and(|target| target == &id) {
                    return Ok(peripheral);
                }
                let name_matches = properties
                    .as_ref()
                    .and_then(|value| value.local_name.as_deref())
                    .is_some_and(|name| name.to_ascii_lowercase().contains("gdx"));
                let service_matches = properties
                    .as_ref()
                    .is_some_and(|value| value.services.contains(&SERVICE_UUID));
                if fallback.is_none() && (name_matches || service_matches) {
                    fallback = Some(peripheral);
                }
            }
        }
        fallback.ok_or_else(|| anyhow!("no Go Direct peripheral found"))
    }

    async fn connected_session(
        &self,
        peripheral: &Peripheral,
        sender: &mpsc::Sender<DriverEvent>,
        cancellation: &CancellationToken,
        backoff: &mut ReconnectBackoff,
    ) -> Result<()> {
        let source_id = peripheral.id().to_string();
        let connect_result = tokio::select! {
            () = cancellation.cancelled() => return Ok(()),
            result = tokio::time::timeout(BLE_OPERATION_TIMEOUT, peripheral.connect()) => {
                result.context("connect Go Direct peripheral timed out")?
            }
        };
        connect_result.context("connect Go Direct peripheral")?;

        let mut subscribed: Option<Characteristic> = None;
        let mut cleanup: Option<(Characteristic, CommandEncoder)> = None;
        let session_result: Result<()> = async {
            tokio::select! {
                () = cancellation.cancelled() => return Ok(()),
                result = tokio::time::timeout(BLE_OPERATION_TIMEOUT, peripheral.discover_services()) => {
                    result.context("discover Go Direct services timed out")??;
                }
            }
            let characteristics = peripheral.characteristics();
            let command = characteristics
                .iter()
                .find(|characteristic| characteristic.uuid == COMMAND_UUID)
                .cloned()
                .ok_or_else(|| anyhow!("Go Direct command characteristic is absent"))?;
            let response = characteristics
                .iter()
                .find(|characteristic| characteristic.uuid == RESPONSE_UUID)
                .cloned()
                .ok_or_else(|| anyhow!("Go Direct response characteristic is absent"))?;
            tokio::select! {
                () = cancellation.cancelled() => return Ok(()),
                result = tokio::time::timeout(BLE_OPERATION_TIMEOUT, peripheral.subscribe(&response)) => {
                    result.context("subscribe Go Direct responses timed out")??;
                }
            }
            subscribed = Some(response.clone());
            let mut notifications = cancellable_timeout(
                cancellation,
                BLE_OPERATION_TIMEOUT,
                "open Go Direct notification stream",
                peripheral.notifications(),
            )
            .await?;
            let mut assembler = PacketAssembler::default();
            let mut encoder = CommandEncoder::default();

            send_command(
                peripheral,
                &command,
                &response,
                &mut notifications,
                &mut assembler,
                &encoder.initialize()?,
                cancellation,
            )
            .await?;
            let status = parse_device_status(
                &send_command(
                    peripheral,
                    &command,
                    &response,
                    &mut notifications,
                    &mut assembler,
                    &encoder.get_status()?,
                    cancellation,
                )
                .await?,
            )?;
            let identity = parse_device_identity(
                &send_command(
                    peripheral,
                    &command,
                    &response,
                    &mut notifications,
                    &mut assembler,
                    &encoder.get_device_info()?,
                    cancellation,
                )
                .await?,
            )?;
            let default_sensors = parse_sensor_mask(
                &send_command(
                    peripheral,
                    &command,
                    &response,
                    &mut notifications,
                    &mut assembler,
                    &encoder.get_default_sensors()?,
                    cancellation,
                )
                .await?,
            )?;
            let available_sensors = parse_sensor_mask(
                &send_command(
                    peripheral,
                    &command,
                    &response,
                    &mut notifications,
                    &mut assembler,
                    &encoder.get_available_sensors()?,
                    cancellation,
                )
                .await?,
            )?;
            if available_sensors & (1_u32 << self.channel) == 0 {
                bail!(
                    "Go Direct channel {} is unavailable (mask {available_sensors:#010x})",
                    self.channel
                );
            }
            let sensor = parse_sensor_info(
                &send_command(
                    peripheral,
                    &command,
                    &response,
                    &mut notifications,
                    &mut assembler,
                    &encoder.get_sensor_info(self.channel)?,
                    cancellation,
                )
                .await?,
            )?;
            if sensor.channel != self.channel {
                bail!(
                    "Go Direct returned metadata for channel {} while channel {} was requested",
                    sensor.channel,
                    self.channel
                );
            }
            let requested_period_us = self
                .period
                .as_micros()
                .min(u128::from(u64::MAX)) as u64;
            if requested_period_us < u64::from(sensor.minimum_period_us)
                || (sensor.maximum_period_us != 0
                    && requested_period_us > sensor.maximum_period_us)
            {
                bail!(
                    "requested Go Direct period {requested_period_us} us is outside channel {} range {}..={} us",
                    self.channel,
                    sensor.minimum_period_us,
                    sensor.maximum_period_us
                );
            }
            info!(
                id = %source_id,
                order_code = %identity.order_code,
                serial = %identity.serial_number,
                device_name = %identity.name,
                main_firmware = %status.main_firmware,
                ble_firmware = %status.ble_firmware,
                battery_percent = status.battery_percent,
                channel = sensor.channel,
                sensor = %sensor.description,
                unit = %sensor.unit,
                default_sensors = format_args!("{default_sensors:#010x}"),
                available_sensors = format_args!("{available_sensors:#010x}"),
                "Go Direct metadata loaded"
            );

            send_command(
                peripheral,
                &command,
                &response,
                &mut notifications,
                &mut assembler,
                &encoder.set_measurement_period(
                    self.period.as_millis().min(u128::from(u32::MAX)) as u32,
                )?,
                cancellation,
            )
            .await?;
            send_command(
                peripheral,
                &command,
                &response,
                &mut notifications,
                &mut assembler,
                &encoder.start_measurements(1_u32 << self.channel)?,
                cancellation,
            )
            .await?;
            cleanup = Some((command, encoder));
            backoff.reset();

            if !send_driver_event(
                sender,
                cancellation,
                DriverEvent::Status {
                    source_id: source_id.clone(),
                    state: DriverConnectionState::Connected,
                    detail: format!(
                        "{} channel {}: {} [{}] at {} ms",
                        identity.name,
                        sensor.channel,
                        sensor.description,
                        sensor.unit,
                        self.period.as_millis()
                    ),
                },
            )
            .await?
            {
                return Ok(());
            }

            let notification_timeout = NOTIFICATION_TIMEOUT_MINIMUM.max(
                self.period
                    .checked_mul(10)
                    .unwrap_or(NOTIFICATION_TIMEOUT_MINIMUM),
            );
            let silence = tokio::time::sleep(notification_timeout);
            tokio::pin!(silence);
            'session: loop {
                tokio::select! {
                    () = cancellation.cancelled() => break Ok(()),
                    () = &mut silence => bail!(
                        "Go Direct notifications silent for {} ms",
                        notification_timeout.as_millis()
                    ),
                    notification = notifications.next() => {
                        let notification = notification
                            .ok_or_else(|| anyhow!("Go Direct notification stream ended"))?;
                        if notification.uuid != RESPONSE_UUID {
                            continue;
                        }
                        silence.as_mut().reset(tokio::time::Instant::now() + notification_timeout);
                        let mut packet = assembler.push(&notification.value)?;
                        while let Some(complete) = packet {
                            match parse_measurements(&complete) {
                                Ok(measurements) => {
                                    for measurement in measurements
                                        .into_iter()
                                        .filter(|measurement| measurement.channel == self.channel)
                                    {
                                        let quality_flags = if measurement.value.is_finite() {
                                            0
                                        } else {
                                            quality::SOURCE_INVALID
                                        };
                                        if !send_driver_event(sender, cancellation, DriverEvent::Measurement {
                                            stream: StreamKind::RespirationForce,
                                            source_id: source_id.clone(),
                                            device_time_ns: None,
                                            value: measurement.value,
                                            quality_flags,
                                        }).await? {
                                            break 'session Ok(());
                                        }
                                    }
                                }
                                Err(ProtocolError::Metadata(_)) => {}
                                Err(ProtocolError::NotMeasurement) => {
                                    warn!("unexpected Go Direct command response during measurement stream");
                                }
                                Err(error) => break 'session Err(error.into()),
                            }
                            packet = assembler.take_ready()?;
                        }
                    }
                }
            }
        }
        .await;

        if let Some((command, mut encoder)) = cleanup {
            if let Ok(stop) = encoder.stop_measurements() {
                let _ = tokio::time::timeout(
                    CLEANUP_TIMEOUT,
                    write_packet(peripheral, &command, &stop),
                )
                .await;
            }
            if let Ok(disconnect) = encoder.disconnect() {
                let _ = tokio::time::timeout(
                    CLEANUP_TIMEOUT,
                    write_packet(peripheral, &command, &disconnect),
                )
                .await;
            }
        }
        if let Some(response) = &subscribed {
            let _ = tokio::time::timeout(CLEANUP_TIMEOUT, peripheral.unsubscribe(response)).await;
        }
        let _ = tokio::time::timeout(CLEANUP_TIMEOUT, peripheral.disconnect()).await;
        session_result
    }
}

impl Default for GoDirectDriver {
    fn default() -> Self {
        Self::respiration_belt()
    }
}

#[async_trait]
impl SensorDriver for GoDirectDriver {
    fn descriptor(&self) -> DeviceDescriptor {
        DeviceDescriptor {
            id: self
                .target_id
                .clone()
                .unwrap_or_else(|| "godirect:auto".to_owned()),
            name: "Go Direct Respiration Belt".to_owned(),
            kind: DeviceKind::GoDirect,
        }
    }

    async fn run(
        self: Box<Self>,
        sender: mpsc::Sender<DriverEvent>,
        cancellation: CancellationToken,
    ) -> Result<()> {
        let mut backoff =
            ReconnectBackoff::new(Duration::from_millis(500), Duration::from_secs(30));
        while !cancellation.is_cancelled() {
            if !send_driver_event(
                &sender,
                &cancellation,
                DriverEvent::Status {
                    source_id: self.descriptor().id,
                    state: DriverConnectionState::Connecting,
                    detail: "discovering Go Direct sensor".to_owned(),
                },
            )
            .await?
            {
                break;
            }
            let result = match self.find_peripheral(&cancellation).await {
                Ok(peripheral) => {
                    info!(id = %peripheral.id(), "found Go Direct peripheral");
                    self.connected_session(&peripheral, &sender, &cancellation, &mut backoff)
                        .await
                }
                Err(error) => Err(error),
            };
            if cancellation.is_cancelled() {
                break;
            }
            let retry = backoff.next_delay();
            if let Err(error) = &result {
                warn!(%error, ?retry, "Go Direct session failed; retrying");
            }
            if !send_driver_event(
                &sender,
                &cancellation,
                DriverEvent::Status {
                    source_id: self.descriptor().id,
                    state: DriverConnectionState::Reconnecting,
                    detail: format!("reconnecting in {} ms", retry.as_millis()),
                },
            )
            .await?
            {
                break;
            }
            tokio::select! {
                () = cancellation.cancelled() => break,
                () = tokio::time::sleep(retry) => {}
            }
        }
        Ok(())
    }
}

async fn write_packet(
    peripheral: &Peripheral,
    characteristic: &Characteristic,
    packet: &[u8],
) -> Result<()> {
    for chunk in packet.chunks(20) {
        peripheral
            .write(characteristic, chunk, WriteType::WithoutResponse)
            .await
            .context("write Go Direct command fragment")?;
    }
    Ok(())
}

async fn send_command<S>(
    peripheral: &Peripheral,
    command_characteristic: &Characteristic,
    response_characteristic: &Characteristic,
    notifications: &mut S,
    assembler: &mut PacketAssembler,
    packet: &[u8],
    cancellation: &CancellationToken,
) -> Result<Vec<u8>>
where
    S: Stream<Item = ValueNotification> + Unpin,
{
    let command = *packet.get(4).ok_or(ProtocolError::Truncated)?;
    let counter = *packet.get(2).ok_or(ProtocolError::Truncated)?;
    tokio::select! {
        () = cancellation.cancelled() => bail!("Go Direct session cancelled"),
        result = tokio::time::timeout(Duration::from_secs(5), write_packet(
            peripheral,
            command_characteristic,
            packet,
        )) => result.context("Go Direct command write timed out")??,
    }
    let response = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let complete = if let Some(buffered) = assembler.take_ready()? {
                buffered
            } else {
                let notification = tokio::select! {
                    () = cancellation.cancelled() => bail!("Go Direct session cancelled"),
                    notification = notifications.next() => notification
                        .ok_or_else(|| anyhow!("Go Direct notification stream ended"))?,
                };
                if notification.uuid != response_characteristic.uuid {
                    continue;
                }
                let Some(complete) = assembler.push(&notification.value)? else {
                    continue;
                };
                complete
            };
            if complete.first() == Some(&MEASUREMENT_PACKET) {
                continue;
            }
            if complete.get(4) == Some(&command) && complete.get(5) == Some(&counter) {
                break Ok::<_, anyhow::Error>(complete);
            }
        }
    })
    .await
    .context("Go Direct command timed out")??;
    Ok(response.get(6..).ok_or(ProtocolError::Truncated)?.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_reference_period_and_start_commands() {
        let mut encoder = CommandEncoder::default();
        let initialize = encoder.initialize().unwrap();
        assert_eq!(initialize.len(), 25);
        assert_eq!(initialize[2], 0xfe);
        assert_eq!(initialize[4], 0x1a);
        let period = encoder.set_measurement_period(100).unwrap();
        assert_eq!(&period[0..4], &[0x58, 15, 0xfd, period[3]]);
        assert_eq!(&period[7..11], &100_000_u32.to_le_bytes());
        assert_eq!(period[3], packet_checksum(&period));

        let start = encoder.start_measurements(1 << 2).unwrap();
        assert_eq!(start[2], 0xfc);
        assert_eq!(&start[7..11], &(1_u32 << 2).to_le_bytes());
    }

    #[test]
    fn parses_normal_float_measurements_in_channel_order() {
        let mut packet = vec![0x20, 0, 0, 0, 0x06, 0b0000_0101, 0, 1, 0];
        packet.extend_from_slice(&12.5_f32.to_le_bytes());
        packet.extend_from_slice(&(-2.0_f32).to_le_bytes());
        packet[1] = packet.len() as u8;
        assert_eq!(
            parse_measurements(&packet).unwrap(),
            vec![
                ChannelMeasurement {
                    channel: 0,
                    value: 12.5,
                },
                ChannelMeasurement {
                    channel: 2,
                    value: -2.0,
                },
            ]
        );
    }

    #[test]
    fn assembles_fragmented_packet() {
        let mut assembler = PacketAssembler::default();
        assert_eq!(assembler.push(&[0x20, 6, 1]).unwrap(), None);
        assert_eq!(
            assembler.push(&[2, 3, 4]).unwrap(),
            Some(vec![0x20, 6, 1, 2, 3, 4])
        );

        let combined = [0x20, 5, 0, 0, 0x0c, 0x20, 5, 0, 0, 0x0d];
        assert_eq!(
            assembler.push(&combined).unwrap(),
            Some(combined[..5].to_vec())
        );
        assert_eq!(
            assembler.take_ready().unwrap(),
            Some(combined[5..].to_vec())
        );
    }

    #[test]
    fn command_encoder_rejects_packets_larger_than_wire_length() {
        assert_eq!(
            CommandEncoder::default().encode(&[0; 252]),
            Err(ProtocolError::CommandTooLong(252))
        );
    }

    #[test]
    fn parses_wide_single_float_and_single_integer_layouts() {
        let mut wide = vec![0x20, 0, 0, 0, 0x07, 0, 0, 0, 0x80, 1, 0];
        wide.extend_from_slice(&3.25_f32.to_le_bytes());
        wide[1] = wide.len() as u8;
        assert_eq!(
            parse_measurements(&wide).unwrap(),
            vec![ChannelMeasurement {
                channel: 31,
                value: 3.25
            }]
        );

        let mut single_float = vec![0x20, 0, 0, 0, 0x0a, 0, 7, 2];
        single_float.extend_from_slice(&1.5_f32.to_le_bytes());
        single_float.extend_from_slice(&(-4.0_f32).to_le_bytes());
        single_float[1] = single_float.len() as u8;
        assert_eq!(
            parse_measurements(&single_float).unwrap(),
            vec![
                ChannelMeasurement {
                    channel: 7,
                    value: 1.5
                },
                ChannelMeasurement {
                    channel: 7,
                    value: -4.0
                }
            ]
        );

        let mut single_integer = vec![0x20, 0, 0, 0, 0x0b, 0, 2, 1];
        single_integer.extend_from_slice(&(-123_i32).to_le_bytes());
        single_integer[1] = single_integer.len() as u8;
        assert_eq!(
            parse_measurements(&single_integer).unwrap(),
            vec![ChannelMeasurement {
                channel: 2,
                value: -123.0
            }]
        );

        single_float[4] = 0x08;
        assert_eq!(parse_measurements(&single_float).unwrap().len(), 2);
        single_integer[4] = 0x09;
        assert_eq!(
            parse_measurements(&single_integer).unwrap()[0].value,
            -123.0
        );
    }

    #[test]
    fn parses_reference_metadata_payloads() {
        let mut status = [0_u8; 12];
        status[2] = 2;
        status[3] = 7;
        status[4..6].copy_from_slice(&123_u16.to_le_bytes());
        status[6] = 1;
        status[7] = 9;
        status[8..10].copy_from_slice(&456_u16.to_le_bytes());
        status[10] = 83;
        status[11] = 1;
        assert_eq!(
            parse_device_status(&status).unwrap(),
            DeviceStatus {
                main_firmware: "2.7.123".to_owned(),
                ble_firmware: "1.9.456".to_owned(),
                battery_percent: 83,
                charging_state: 1,
            }
        );

        let mut identity = [0_u8; 64];
        identity[0..7].copy_from_slice(b"GDX-RB\0");
        identity[16..23].copy_from_slice(b"123456\0");
        identity[32..49].copy_from_slice(b"Respiration Belt\0");
        assert_eq!(
            parse_device_identity(&identity).unwrap(),
            DeviceIdentity {
                order_code: "GDX-RB".to_owned(),
                serial_number: "123456".to_owned(),
                name: "Respiration Belt".to_owned(),
            }
        );

        let mut sensor = [0_u8; 148];
        sensor[0] = 1;
        sensor[2..6].copy_from_slice(&42_u32.to_le_bytes());
        sensor[6] = 0;
        sensor[7] = 0;
        sensor[8..26].copy_from_slice(b"Respiration Force\0");
        sensor[68..70].copy_from_slice(b"N\0");
        sensor[100..108].copy_from_slice(&0.01_f64.to_le_bytes());
        sensor[108..116].copy_from_slice(&(-50.0_f64).to_le_bytes());
        sensor[116..124].copy_from_slice(&50.0_f64.to_le_bytes());
        sensor[124..128].copy_from_slice(&10_000_u32.to_le_bytes());
        sensor[128..136].copy_from_slice(&1_000_000_u64.to_le_bytes());
        sensor[136..140].copy_from_slice(&100_000_u32.to_le_bytes());
        sensor[140..144].copy_from_slice(&1_000_u32.to_le_bytes());
        sensor[144..148].copy_from_slice(&4_u32.to_le_bytes());
        assert_eq!(
            parse_sensor_info(&sensor).unwrap(),
            SensorInfo {
                channel: 1,
                sensor_id: 42,
                numeric_type: 0,
                sampling_mode: 0,
                description: "Respiration Force".to_owned(),
                unit: "N".to_owned(),
                uncertainty: 0.01,
                minimum: -50.0,
                maximum: 50.0,
                minimum_period_us: 10_000,
                maximum_period_us: 1_000_000,
                typical_period_us: 100_000,
                period_granularity_us: 1_000,
                mutual_exclusion_mask: 4,
            }
        );
    }

    #[test]
    fn rejects_inconsistent_measurement_lengths_and_value_counts() {
        assert_eq!(
            parse_measurements(&[0x20, 5, 0, 0, 0x0c]),
            Err(ProtocolError::Metadata(0x0c))
        );
        assert_eq!(
            parse_measurements(&[0x20, 5, 0, 0, 0x06, 0]),
            Err(ProtocolError::LengthMismatch {
                declared: 5,
                actual: 6
            })
        );
        assert_eq!(
            parse_measurements(&[0x20, 9, 0, 0, 0x06, 1, 0, 1, 0]),
            Err(ProtocolError::ValueCount)
        );
        assert_eq!(
            parse_measurements(&[0x20, 13, 0, 0, 0x06, 1, 0, 0, 0, 0, 0, 0, 0]),
            Err(ProtocolError::ValueCount)
        );
        assert_eq!(parse_sensor_info(&[0; 147]), Err(ProtocolError::Truncated));
    }
}
