//! Exercise the real serial driver through a pseudo-terminal, RPC, and recording.
#![cfg(unix)]

use std::fs;
use std::sync::Arc;
use std::time::Duration;

use kasina_devices::SensorDriver;
use kasina_domain::quality;
use kasina_protocol::client_hello;
use kasina_protocol::v1::kasina_client::KasinaClient;
use kasina_protocol::v1::{
    DeviceKind, SamplesSinceRequest, StartRecordingRequest, StopRecordingRequest, StreamCursor,
    StreamKind, SubscribeRequest,
};
use kasina_service::{KasinaRpc, ServiceState, authenticated_request, serve};
use kasina_thoughtstream::ThoughtStreamDriver;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio_serial::{SerialPort, SerialStream};
use tokio_util::sync::CancellationToken;

#[tokio::test]
#[cfg_attr(
    target_os = "macos",
    ignore = "macOS pseudo-terminals cannot apply the real serial driver's IOSSIOSPEED baud rate"
)]
async fn serial_packets_reach_history_subscriptions_and_recordings_across_client_restart() {
    tokio::time::timeout(Duration::from_secs(10), exercise_serial_pipeline())
        .await
        .expect("serial pipeline must complete promptly");
}

async fn exercise_serial_pipeline() {
    let (mut master, slave) = SerialStream::pair().unwrap();
    let path = slave.name().unwrap();
    drop(slave);
    let driver = Box::new(ThoughtStreamDriver::new(Some(path.clone())));
    let directory = tempfile::tempdir().unwrap();
    let state = ServiceState::new_multi_with_recordings(
        Duration::from_secs(30),
        vec![driver.descriptor()],
        directory.path().to_owned(),
    )
    .unwrap();
    let cancel = CancellationToken::new();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let token = "thoughtstream-test";
    let server = tokio::spawn(serve(
        listener,
        KasinaRpc::new(Arc::clone(&state), token),
        cancel.child_token(),
    ));
    let mut client = KasinaClient::connect(endpoint.clone()).await.unwrap();
    client
        .start_recording(
            authenticated_request(
                StartRecordingRequest {
                    client: Some(client_hello("test", "0")),
                    label: "serial fixture".to_owned(),
                    notes: String::new(),
                },
                token,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let acquisition = tokio::spawn(Arc::clone(&state).run_driver(driver, cancel.child_token()));
    let writer_cancel = CancellationToken::new();
    let writer_stop = writer_cancel.clone();
    let writer = tokio::spawn(async move {
        // Valid packet: ADC 10000, probe error + low battery + new data + recalculation.
        let frame = [0xa3, 0x5b, 8, 0x27, 0x10, 15, 1, 0x4c];
        loop {
            tokio::select! {
                () = writer_stop.cancelled() => break,
                () = tokio::time::sleep(Duration::from_millis(20)) => {
                    master.write_all(&frame[..3]).await.unwrap();
                    master.write_all(&frame[3..]).await.unwrap();
                }
            }
        }
        master
    });
    let mut subscription = client
        .subscribe_samples(
            authenticated_request(
                SubscribeRequest {
                    client: Some(client_hello("test", "0")),
                    streams: vec![StreamKind::SkinResistance as i32],
                },
                token,
            )
            .unwrap(),
        )
        .await
        .unwrap()
        .into_inner();
    let first = loop {
        let batch = subscription.message().await.unwrap().unwrap();
        if let Some(sample) = batch.samples.first() {
            break sample.clone();
        }
    };
    assert_eq!(first.stream, StreamKind::SkinResistance as i32);
    assert_eq!(first.value, 300_001.0);
    assert_eq!(first.unit, "ohm");
    assert_eq!(first.source_id, format!("thoughtstream:{path}"));
    assert_eq!(first.device_time_ns, None);
    assert_eq!(
        first.quality_flags,
        quality::PROBE_ERROR
            | quality::SOURCE_INVALID
            | quality::LOW_BATTERY
            | quality::RECALIBRATED
    );
    let devices = client
        .list_devices(authenticated_request(client_hello("test", "0"), token).unwrap())
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        devices.devices[0].device.as_ref().unwrap().kind,
        DeviceKind::ThoughtStream as i32
    );
    drop(subscription);
    drop(client);
    let mut restarted = KasinaClient::connect(endpoint).await.unwrap();
    let recovered = loop {
        let batch = restarted
            .get_samples_since(
                authenticated_request(
                    SamplesSinceRequest {
                        client: Some(client_hello("restarted-test", "0")),
                        cursors: vec![
                            StreamCursor {
                                stream: StreamKind::SkinResistance as i32,
                                after_sequence: first.sequence,
                            },
                            StreamCursor {
                                stream: StreamKind::ThoughtStreamAdc as i32,
                                after_sequence: 0,
                            },
                        ],
                    },
                    token,
                )
                .unwrap(),
            )
            .await
            .unwrap()
            .into_inner();
        if batch
            .samples
            .iter()
            .any(|s| s.stream == StreamKind::SkinResistance as i32)
        {
            break batch;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert!(recovered.gaps.is_empty());
    assert!(
        recovered
            .samples
            .iter()
            .filter(|s| s.stream == StreamKind::SkinResistance as i32)
            .all(|s| s.sequence > first.sequence)
    );
    assert!(
        recovered
            .samples
            .iter()
            .any(|s| s.stream == StreamKind::ThoughtStreamAdc as i32
                && s.value == 10_000.0
                && s.unit == "count")
    );
    writer_cancel.cancel();
    let _master = writer.await.unwrap();
    let completed = restarted
        .stop_recording(
            authenticated_request(
                StopRecordingRequest {
                    client: Some(client_hello("test", "0")),
                },
                token,
            )
            .unwrap(),
        )
        .await
        .unwrap()
        .into_inner();
    let samples =
        fs::read_to_string(std::path::Path::new(&completed.directory).join("samples.jsonl"))
            .unwrap();
    let rows: Vec<serde_json::Value> = samples
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(rows.len() as u64, completed.sample_count);
    assert_eq!(completed.dropped_samples, 0);
    for (stream, unit) in [("thoughtstream_adc", "count"), ("skin_resistance", "ohm")] {
        let row = rows.iter().find(|row| row["stream"] == stream).unwrap();
        assert_eq!(row["unit"], unit);
        assert_eq!(row["quality_flags"], first.quality_flags);
    }
    let metadata: serde_json::Value = serde_json::from_slice(
        &fs::read(std::path::Path::new(&completed.directory).join("metadata.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(metadata["devices"][0]["kind"], "thoughtstream");
    cancel.cancel();
    acquisition.await.unwrap().unwrap();
    server.await.unwrap().unwrap();
}
