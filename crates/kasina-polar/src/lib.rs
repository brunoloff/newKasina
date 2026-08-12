//! Polar Heart Rate Service parsing and a reconnecting `btleplug` acquisition driver.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use btleplug::api::{Central, Manager as _, Peripheral as _, ScanFilter};
use btleplug::platform::{Manager, Peripheral};
use futures::StreamExt;
use kasina_devices::{DeviceDescriptor, DeviceKind, DriverEvent, SensorDriver};
use kasina_domain::StreamKind;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use uuid::Uuid;

/// Bluetooth SIG Heart Rate service UUID.
pub const HEART_RATE_SERVICE_UUID: Uuid =
    Uuid::from_u128(0x0000_180d_0000_1000_8000_0080_5f9b_34fb);
/// Bluetooth SIG Heart Rate Measurement characteristic UUID.
pub const HEART_RATE_MEASUREMENT_UUID: Uuid =
    Uuid::from_u128(0x0000_2a37_0000_1000_8000_0080_5f9b_34fb);

/// Fully decoded Heart Rate Measurement notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeartRateMeasurement {
    /// Heart rate in beats per minute.
    pub heart_rate_bpm: u16,
    /// Optional accumulated energy expenditure in kilojoules.
    pub energy_expended_kj: Option<u16>,
    /// Every RR interval present in this notification, in native 1/1024-second ticks.
    pub rr_intervals_1024: Vec<u16>,
    /// Sensor contact state when the device reports that feature.
    pub sensor_contact: Option<bool>,
}

/// Malformed Heart Rate Measurement payload.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum HeartRateParseError {
    /// A field advertised by the flags did not fit in the payload.
    #[error("heart-rate notification ended while reading {field}")]
    Truncated {
        /// Field that could not be read.
        field: &'static str,
    },
    /// A trailing byte cannot form a complete RR interval.
    #[error("heart-rate notification has an odd number of RR bytes")]
    OddRrBytes,
}

fn take_u8(
    payload: &[u8],
    offset: &mut usize,
    field: &'static str,
) -> Result<u8, HeartRateParseError> {
    let value = *payload
        .get(*offset)
        .ok_or(HeartRateParseError::Truncated { field })?;
    *offset += 1;
    Ok(value)
}

fn take_u16_le(
    payload: &[u8],
    offset: &mut usize,
    field: &'static str,
) -> Result<u16, HeartRateParseError> {
    let bytes = payload
        .get(*offset..offset.saturating_add(2))
        .ok_or(HeartRateParseError::Truncated { field })?;
    *offset += 2;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

/// Parse the Bluetooth SIG Heart Rate Measurement format used by the Polar H10.
pub fn parse_heart_rate_measurement(
    payload: &[u8],
) -> Result<HeartRateMeasurement, HeartRateParseError> {
    let mut offset = 0;
    let flags = take_u8(payload, &mut offset, "flags")?;
    let heart_rate_bpm = if flags & 0x01 == 0 {
        u16::from(take_u8(payload, &mut offset, "8-bit heart rate")?)
    } else {
        take_u16_le(payload, &mut offset, "16-bit heart rate")?
    };

    let sensor_contact = if flags & 0x04 == 0 {
        None
    } else {
        Some(flags & 0x02 != 0)
    };
    let energy_expended_kj = if flags & 0x08 == 0 {
        None
    } else {
        Some(take_u16_le(payload, &mut offset, "energy expenditure")?)
    };

    let mut rr_intervals_1024 = Vec::new();
    if flags & 0x10 != 0 {
        if !(payload.len() - offset).is_multiple_of(2) {
            return Err(HeartRateParseError::OddRrBytes);
        }
        while offset < payload.len() {
            rr_intervals_1024.push(take_u16_le(payload, &mut offset, "RR interval")?);
        }
    }

    Ok(HeartRateMeasurement {
        heart_rate_bpm,
        energy_expended_kj,
        rr_intervals_1024,
        sensor_contact,
    })
}

/// Convert native Heart Rate Service RR ticks to microseconds without early rounding.
#[must_use]
pub fn rr_ticks_to_microseconds(ticks: u16) -> f64 {
    f64::from(ticks) * 1_000_000.0 / 1024.0
}

/// Polar H10 acquisition configuration.
#[derive(Debug, Clone)]
pub struct PolarDriver {
    target_id: Option<String>,
    scan_duration: Duration,
}

impl PolarDriver {
    /// Search for any Polar heart-rate peripheral.
    #[must_use]
    pub fn any() -> Self {
        Self {
            target_id: None,
            scan_duration: Duration::from_secs(5),
        }
    }

    /// Prefer a previously saved platform peripheral identifier.
    #[must_use]
    pub fn with_target_id(target_id: impl Into<String>) -> Self {
        Self {
            target_id: Some(target_id.into()),
            ..Self::any()
        }
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
            .context("start Polar scan")?;
        tokio::time::sleep(self.scan_duration).await;

        for peripheral in adapter.peripherals().await? {
            let id = peripheral.id().to_string();
            let properties = peripheral.properties().await?;
            let name_matches = properties
                .as_ref()
                .and_then(|value| value.local_name.as_deref())
                .is_some_and(|name| name.to_ascii_lowercase().contains("polar"));
            let service_matches = properties
                .as_ref()
                .is_some_and(|value| value.services.contains(&HEART_RATE_SERVICE_UUID));
            let id_matches = self.target_id.as_ref().is_some_and(|target| target == &id);
            if id_matches || name_matches || service_matches {
                return Ok(peripheral);
            }
        }
        bail!("no Polar heart-rate peripheral found")
    }

    async fn connected_session(
        &self,
        peripheral: &Peripheral,
        sender: &mpsc::Sender<DriverEvent>,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        let source_id = peripheral.id().to_string();
        peripheral
            .connect()
            .await
            .context("connect Polar peripheral")?;
        peripheral
            .discover_services()
            .await
            .context("discover Polar services")?;
        let characteristic = peripheral
            .characteristics()
            .into_iter()
            .find(|characteristic| characteristic.uuid == HEART_RATE_MEASUREMENT_UUID)
            .ok_or_else(|| anyhow!("Polar Heart Rate Measurement characteristic is absent"))?;
        peripheral
            .subscribe(&characteristic)
            .await
            .context("subscribe Polar HR")?;
        sender
            .send(DriverEvent::Status {
                source_id: source_id.clone(),
                detail: "connected".to_owned(),
            })
            .await?;
        let mut notifications = peripheral.notifications().await?;
        loop {
            tokio::select! {
                () = cancellation.cancelled() => return Ok(()),
                notification = notifications.next() => {
                    let notification = notification.ok_or_else(|| anyhow!("Polar notification stream ended"))?;
                    if notification.uuid != HEART_RATE_MEASUREMENT_UUID {
                        continue;
                    }
                    let measurement = parse_heart_rate_measurement(&notification.value)?;
                    sender.send(DriverEvent::Measurement {
                        stream: StreamKind::HeartRate,
                        source_id: source_id.clone(),
                        device_time_ns: None,
                        value: f64::from(measurement.heart_rate_bpm),
                        quality_flags: 0,
                    }).await?;
                    for ticks in measurement.rr_intervals_1024 {
                        sender.send(DriverEvent::Measurement {
                            stream: StreamKind::RrInterval,
                            source_id: source_id.clone(),
                            device_time_ns: None,
                            value: rr_ticks_to_microseconds(ticks),
                            quality_flags: 0,
                        }).await?;
                    }
                }
            }
        }
    }
}

impl Default for PolarDriver {
    fn default() -> Self {
        Self::any()
    }
}

#[async_trait]
impl SensorDriver for PolarDriver {
    fn descriptor(&self) -> DeviceDescriptor {
        DeviceDescriptor {
            id: self
                .target_id
                .clone()
                .unwrap_or_else(|| "polar:auto".to_owned()),
            name: "Polar H10".to_owned(),
            kind: DeviceKind::Polar,
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
                    info!(id = %peripheral.id(), "found Polar peripheral");
                    self.connected_session(&peripheral, &sender, &cancellation)
                        .await
                }
                Err(error) => Err(error),
            };
            if cancellation.is_cancelled() {
                break;
            }
            if let Err(error) = &result {
                warn!(%error, ?retry, "Polar session failed; retrying");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_eight_bit_hr_without_optionals() {
        assert_eq!(
            parse_heart_rate_measurement(&[0x00, 72]).unwrap(),
            HeartRateMeasurement {
                heart_rate_bpm: 72,
                energy_expended_kj: None,
                rr_intervals_1024: Vec::new(),
                sensor_contact: None,
            }
        );
    }

    #[test]
    fn parses_all_fields_and_multiple_rr_intervals() {
        let value =
            parse_heart_rate_measurement(&[0x1f, 0x2c, 0x01, 0x34, 0x12, 0x00, 0x04, 0x80, 0x03])
                .unwrap();
        assert_eq!(value.heart_rate_bpm, 300);
        assert_eq!(value.energy_expended_kj, Some(0x1234));
        assert_eq!(value.rr_intervals_1024, vec![1024, 896]);
        assert_eq!(value.sensor_contact, Some(true));
        assert_eq!(rr_ticks_to_microseconds(1024), 1_000_000.0);
    }

    #[test]
    fn rejects_truncated_and_odd_frames() {
        assert_eq!(
            parse_heart_rate_measurement(&[]),
            Err(HeartRateParseError::Truncated { field: "flags" })
        );
        assert_eq!(
            parse_heart_rate_measurement(&[0x10, 60, 0xff]),
            Err(HeartRateParseError::OddRrBytes)
        );
        assert_eq!(
            parse_heart_rate_measurement(&[0x09, 60, 1]),
            Err(HeartRateParseError::Truncated {
                field: "energy expenditure"
            })
        );
    }
}
