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
use kasina_devices::{DeviceDescriptor, DeviceKind, DriverEvent, SensorDriver};
use kasina_domain::StreamKind;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

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
    #[must_use]
    pub fn encode(&mut self, subcommand: &[u8]) -> Vec<u8> {
        self.rolling_counter = self.rolling_counter.wrapping_sub(1);
        let mut packet = Vec::with_capacity(4 + subcommand.len());
        packet.extend_from_slice(&[PACKET_HEADER, 0, self.rolling_counter, 0]);
        packet.extend_from_slice(subcommand);
        packet[1] = u8::try_from(packet.len()).expect("Go Direct commands fit in one-byte length");
        packet[3] = packet_checksum(&packet);
        packet
    }

    /// Initialize a newly connected Go Direct session.
    #[must_use]
    pub fn initialize(&mut self) -> Vec<u8> {
        self.encode(INIT)
    }

    /// Set the measurement period in milliseconds (the wire uses microseconds).
    #[must_use]
    pub fn set_measurement_period(&mut self, period_ms: u32) -> Vec<u8> {
        let mut command = SET_MEASUREMENT_PERIOD.to_vec();
        command[3..7].copy_from_slice(&period_ms.saturating_mul(1_000).to_le_bytes());
        self.encode(&command)
    }

    /// Start a selected channel mask.
    #[must_use]
    pub fn start_measurements(&mut self, channel_mask: u32) -> Vec<u8> {
        let mut command = START_MEASUREMENTS.to_vec();
        command[3..7].copy_from_slice(&channel_mask.to_le_bytes());
        self.encode(&command)
    }

    /// Stop all measurements.
    #[must_use]
    pub fn stop_measurements(&mut self) -> Vec<u8> {
        self.encode(STOP_MEASUREMENTS)
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

/// Packet parsing failure.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProtocolError {
    /// Framing is incomplete or inconsistent.
    #[error("truncated Go Direct packet")]
    Truncated,
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
    if expected > declared_length || expected > packet.len() {
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

    async fn find_peripheral(&self) -> Result<Peripheral> {
        let manager = Manager::new().await.context("create Bluetooth manager")?;
        let adapters = manager
            .adapters()
            .await
            .context("enumerate Bluetooth adapters")?;
        let adapter = adapters
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("no Bluetooth adapter is available"))?;
        adapter
            .start_scan(ScanFilter::default())
            .await
            .context("start Go Direct scan")?;
        tokio::time::sleep(self.scan_duration).await;

        for peripheral in adapter.peripherals().await? {
            let id = peripheral.id().to_string();
            let properties = peripheral.properties().await?;
            let name_matches = properties
                .as_ref()
                .and_then(|value| value.local_name.as_deref())
                .is_some_and(|name| name.to_ascii_lowercase().contains("gdx"));
            let service_matches = properties
                .as_ref()
                .is_some_and(|value| value.services.contains(&SERVICE_UUID));
            let id_matches = self.target_id.as_ref().is_some_and(|target| target == &id);
            if id_matches || name_matches || service_matches {
                return Ok(peripheral);
            }
        }
        bail!("no Go Direct peripheral found")
    }

    async fn connected_session(
        &self,
        peripheral: &Peripheral,
        sender: &mpsc::Sender<DriverEvent>,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        peripheral
            .connect()
            .await
            .context("connect Go Direct peripheral")?;
        peripheral
            .discover_services()
            .await
            .context("discover Go Direct services")?;
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
        peripheral
            .subscribe(&response)
            .await
            .context("subscribe Go Direct responses")?;
        let mut notifications = peripheral.notifications().await?;
        let mut assembler = PacketAssembler::default();
        let mut encoder = CommandEncoder::default();

        let source_id = peripheral.id().to_string();
        let session_result: Result<()> = async {
            for packet in [
                encoder.initialize(),
                encoder.set_measurement_period(
                    self.period.as_millis().min(u128::from(u32::MAX)) as u32,
                ),
                encoder.start_measurements(1_u32 << self.channel),
            ] {
                send_command(
                    peripheral,
                    &command,
                    &response,
                    &mut notifications,
                    &mut assembler,
                    &packet,
                    cancellation,
                )
                .await?;
            }

            sender
                .send(DriverEvent::Status {
                    source_id: source_id.clone(),
                    detail: "connected".to_owned(),
                })
                .await?;

            'session: loop {
                tokio::select! {
                    () = cancellation.cancelled() => break Ok(()),
                    notification = notifications.next() => {
                        let notification = notification
                            .ok_or_else(|| anyhow!("Go Direct notification stream ended"))?;
                        if notification.uuid != RESPONSE_UUID {
                            continue;
                        }
                        let mut packet = assembler.push(&notification.value)?;
                        while let Some(complete) = packet {
                            match parse_measurements(&complete) {
                                Ok(measurements) => {
                                    for measurement in measurements
                                        .into_iter()
                                        .filter(|measurement| measurement.channel == self.channel)
                                    {
                                        sender.send(DriverEvent::Measurement {
                                            stream: StreamKind::RespirationForce,
                                            source_id: source_id.clone(),
                                            device_time_ns: None,
                                            value: measurement.value,
                                            quality_flags: 0,
                                        }).await?;
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

        if peripheral.is_connected().await.unwrap_or(false) {
            let stop = encoder.stop_measurements();
            let _ = write_packet(peripheral, &command, &stop).await;
            let _ = peripheral.unsubscribe(&response).await;
            let _ = peripheral.disconnect().await;
        }
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
        let mut retry = Duration::from_millis(500);
        while !cancellation.is_cancelled() {
            let result = match self.find_peripheral().await {
                Ok(peripheral) => {
                    retry = Duration::from_millis(500);
                    info!(id = %peripheral.id(), "found Go Direct peripheral");
                    self.connected_session(&peripheral, &sender, &cancellation)
                        .await
                }
                Err(error) => Err(error),
            };
            if cancellation.is_cancelled() {
                break;
            }
            if let Err(error) = &result {
                warn!(%error, ?retry, "Go Direct session failed; retrying");
            }
            sender
                .send(DriverEvent::Status {
                    source_id: self.descriptor().id,
                    detail: format!("reconnecting in {} ms", retry.as_millis()),
                })
                .await?;
            tokio::select! {
                () = cancellation.cancelled() => break,
                () = tokio::time::sleep(retry) => {}
            }
            retry = (retry * 2).min(Duration::from_secs(30));
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
    write_packet(peripheral, command_characteristic, packet).await?;
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
    let payload = response.get(6..).ok_or(ProtocolError::Truncated)?.to_vec();
    if payload.first().is_some_and(|status| *status != 0) {
        bail!(
            "Go Direct command {command:#04x} returned status {:#04x}",
            payload[0]
        );
    }
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_reference_period_and_start_commands() {
        let mut encoder = CommandEncoder::default();
        let initialize = encoder.initialize();
        assert_eq!(initialize.len(), 25);
        assert_eq!(initialize[2], 0xfe);
        assert_eq!(initialize[4], 0x1a);
        let period = encoder.set_measurement_period(100);
        assert_eq!(&period[0..4], &[0x58, 15, 0xfd, period[3]]);
        assert_eq!(&period[7..11], &100_000_u32.to_le_bytes());
        assert_eq!(period[3], packet_checksum(&period));

        let start = encoder.start_measurements(1 << 2);
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
    }
}
