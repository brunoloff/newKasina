//! Polar Heart Rate Service parsing and a reconnecting `btleplug` acquisition driver.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use btleplug::api::{Central, Characteristic, Manager as _, Peripheral as _, ScanFilter};
use btleplug::platform::{Manager, Peripheral};
use futures::StreamExt;
use kasina_devices::{
    DeviceDescriptor, DeviceKind, DriverConnectionState, DriverEvent, ReconnectBackoff,
    SensorDriver, cancellable_timeout, send_driver_event,
};
use kasina_domain::{StreamKind, quality};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use uuid::Uuid;

const BLE_OPERATION_TIMEOUT: Duration = Duration::from_secs(30);
const NOTIFICATION_TIMEOUT: Duration = Duration::from_secs(5);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(3);

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
                "start Polar scan",
                adapter.start_scan(ScanFilter::default()),
            )
            .await
            {
                Ok(()) => scanning.push(adapter),
                Err(_) if cancellation.is_cancelled() => break,
                Err(error) => warn!(%error, "could not start Polar scan on an adapter"),
            }
        }
        if cancellation.is_cancelled() {
            for adapter in &scanning {
                let _ = tokio::time::timeout(CLEANUP_TIMEOUT, adapter.stop_scan()).await;
            }
            bail!("Polar scan cancelled");
        }
        if scanning.is_empty() {
            bail!("no Bluetooth adapter could start a Polar scan");
        }
        let cancelled = tokio::select! {
            () = cancellation.cancelled() => true,
            () = tokio::time::sleep(self.scan_duration) => false,
        };
        for adapter in &scanning {
            match tokio::time::timeout(CLEANUP_TIMEOUT, adapter.stop_scan()).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => warn!(%error, "could not stop Polar scan"),
                Err(_) => warn!("stopping Polar scan timed out"),
            }
        }
        if cancelled {
            bail!("Polar scan cancelled");
        }

        let mut fallback = None;
        for adapter in &adapters {
            let peripherals = cancellable_timeout(
                cancellation,
                BLE_OPERATION_TIMEOUT,
                "list Polar peripherals",
                adapter.peripherals(),
            )
            .await?;
            for peripheral in peripherals {
                let id = peripheral.id().to_string();
                let properties = match cancellable_timeout(
                    cancellation,
                    BLE_OPERATION_TIMEOUT,
                    "read Polar advertisement properties",
                    peripheral.properties(),
                )
                .await
                {
                    Ok(properties) => properties,
                    Err(error) => {
                        warn!(%error, %id, "could not read Polar advertisement properties");
                        continue;
                    }
                };
                if self.target_id.as_ref().is_some_and(|target| target == &id) {
                    return Ok(peripheral);
                }
                let name_matches = properties
                    .as_ref()
                    .and_then(|value| value.local_name.as_deref())
                    .is_some_and(|name| name.to_ascii_lowercase().contains("polar"));
                let service_matches = properties
                    .as_ref()
                    .is_some_and(|value| value.services.contains(&HEART_RATE_SERVICE_UUID));
                if fallback.is_none() && (name_matches || service_matches) {
                    fallback = Some(peripheral);
                }
            }
        }
        fallback.ok_or_else(|| anyhow!("no Polar heart-rate peripheral found"))
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
                result.context("connect Polar peripheral timed out")?
            }
        };
        connect_result.context("connect Polar peripheral")?;

        let mut subscribed: Option<Characteristic> = None;
        let session_result: Result<()> = async {
            tokio::select! {
                () = cancellation.cancelled() => return Ok(()),
                result = tokio::time::timeout(BLE_OPERATION_TIMEOUT, peripheral.discover_services()) => {
                    result.context("discover Polar services timed out")??;
                }
            }
            let characteristic = peripheral
                .characteristics()
                .into_iter()
                .find(|characteristic| characteristic.uuid == HEART_RATE_MEASUREMENT_UUID)
                .ok_or_else(|| anyhow!("Polar Heart Rate Measurement characteristic is absent"))?;
            tokio::select! {
                () = cancellation.cancelled() => return Ok(()),
                result = tokio::time::timeout(BLE_OPERATION_TIMEOUT, peripheral.subscribe(&characteristic)) => {
                    result.context("subscribe Polar HR timed out")??;
                }
            }
            subscribed = Some(characteristic);
            backoff.reset();
            if !send_driver_event(
                sender,
                cancellation,
                DriverEvent::Status {
                    source_id: source_id.clone(),
                    state: DriverConnectionState::Connected,
                    detail: "subscribed to Heart Rate Measurement".to_owned(),
                },
            )
            .await?
            {
                return Ok(());
            }
            let mut notifications = cancellable_timeout(
                cancellation,
                BLE_OPERATION_TIMEOUT,
                "open Polar notification stream",
                peripheral.notifications(),
            )
            .await?;
            let silence = tokio::time::sleep(NOTIFICATION_TIMEOUT);
            tokio::pin!(silence);
            loop {
                tokio::select! {
                    () = cancellation.cancelled() => break Ok(()),
                    () = &mut silence => bail!("Polar notifications silent for {} seconds", NOTIFICATION_TIMEOUT.as_secs()),
                    notification = notifications.next() => {
                        let notification = notification
                            .ok_or_else(|| anyhow!("Polar notification stream ended"))?;
                        if notification.uuid != HEART_RATE_MEASUREMENT_UUID {
                            continue;
                        }
                        silence.as_mut().reset(tokio::time::Instant::now() + NOTIFICATION_TIMEOUT);
                        let measurement = match parse_heart_rate_measurement(&notification.value) {
                            Ok(measurement) => measurement,
                            Err(error) => {
                                warn!(%error, "discarding malformed Polar notification");
                                continue;
                            }
                        };
                        let hr_quality = if measurement.heart_rate_bpm == 0 {
                            quality::SOURCE_INVALID
                        } else {
                            0
                        };
                        if !send_driver_event(sender, cancellation, DriverEvent::Measurement {
                            stream: StreamKind::HeartRate,
                            source_id: source_id.clone(),
                            device_time_ns: None,
                            value: f64::from(measurement.heart_rate_bpm),
                            quality_flags: hr_quality,
                        }).await? {
                            break Ok(());
                        }
                        for ticks in measurement.rr_intervals_1024 {
                            let rr_quality = if ticks == 0 { quality::SOURCE_INVALID } else { 0 };
                            if !send_driver_event(sender, cancellation, DriverEvent::Measurement {
                                stream: StreamKind::RrInterval,
                                source_id: source_id.clone(),
                                device_time_ns: None,
                                value: rr_ticks_to_microseconds(ticks),
                                quality_flags: rr_quality,
                            }).await? {
                                break;
                            }
                        }
                    }
                }
            }
        }
        .await;

        if let Some(characteristic) = &subscribed {
            let _ =
                tokio::time::timeout(CLEANUP_TIMEOUT, peripheral.unsubscribe(characteristic)).await;
        }
        let _ = tokio::time::timeout(CLEANUP_TIMEOUT, peripheral.disconnect()).await;
        session_result
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
        let mut backoff =
            ReconnectBackoff::new(Duration::from_millis(500), Duration::from_secs(30));
        while !cancellation.is_cancelled() {
            if !send_driver_event(
                &sender,
                &cancellation,
                DriverEvent::Status {
                    source_id: self.descriptor().id,
                    state: DriverConnectionState::Connecting,
                    detail: "discovering Polar heart-rate sensor".to_owned(),
                },
            )
            .await?
            {
                break;
            }
            let result = match self.find_peripheral(&cancellation).await {
                Ok(peripheral) => {
                    info!(id = %peripheral.id(), "found Polar peripheral");
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
                warn!(%error, ?retry, "Polar session failed; retrying");
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
