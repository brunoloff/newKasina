//! Pure Go Direct packet codec plus the boundary for the future BLE transport.
//!
//! The codec implements the wire subset needed by the respiration belt. Hardware
//! discovery and channel selection remain deliberately separate so captured fixtures
//! can validate the protocol without a Bluetooth adapter.

use thiserror::Error;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_reference_period_and_start_commands() {
        let mut encoder = CommandEncoder::default();
        let period = encoder.set_measurement_period(100);
        assert_eq!(&period[0..4], &[0x58, 15, 0xfe, period[3]]);
        assert_eq!(&period[7..11], &100_000_u32.to_le_bytes());
        assert_eq!(period[3], packet_checksum(&period));

        let start = encoder.start_measurements(1 << 2);
        assert_eq!(start[2], 0xfd);
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
