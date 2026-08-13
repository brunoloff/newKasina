use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use kasina_devices::{SensorDriver, SimulatedDriver};
use kasina_protocol::client_hello;
use kasina_protocol::v1::kasina_client::KasinaClient;
use kasina_protocol::v1::{
    ClientHello, RecordingState, SamplesSinceRequest, StartRecordingRequest, StopRecordingRequest,
    StreamCursor, StreamKind,
};
use kasina_service::{KasinaRpc, ServiceState, authenticated_request, serve};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tonic::Code;

#[tokio::test]
async fn client_restart_preserves_sequences_and_recovers_gap_without_duplicates() {
    let token = "integration-test-token";
    let driver: Box<dyn SensorDriver> =
        Box::new(SimulatedDriver::with_period(Duration::from_millis(10)));
    let state = ServiceState::new(Duration::from_secs(30), driver.descriptor());
    let cancellation = CancellationToken::new();
    let acquisition =
        tokio::spawn(Arc::clone(&state).run_driver(driver, cancellation.child_token()));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(serve(
        listener,
        KasinaRpc::new(Arc::clone(&state), token),
        cancellation.child_token(),
    ));

    tokio::time::sleep(Duration::from_millis(90)).await;
    let mut first_client = KasinaClient::connect(format!("http://{address}"))
        .await
        .unwrap();
    let first = history_request(&mut first_client, token, 0).await;
    let first_sequences: Vec<_> = first
        .samples
        .iter()
        .filter(|sample| sample.stream == StreamKind::RespirationForce as i32)
        .map(|sample| sample.sequence)
        .collect();
    assert!(!first_sequences.is_empty());
    assert!(
        first_sequences
            .windows(2)
            .all(|pair| pair[1] == pair[0] + 1)
    );
    let cursor = *first_sequences.last().unwrap();
    drop(first_client);

    tokio::time::sleep(Duration::from_millis(80)).await;
    let mut restarted_client = KasinaClient::connect(format!("http://{address}"))
        .await
        .unwrap();
    let recovered = history_request(&mut restarted_client, token, cursor).await;
    let recovered_sequences: Vec<_> = recovered
        .samples
        .iter()
        .filter(|sample| sample.stream == StreamKind::RespirationForce as i32)
        .map(|sample| sample.sequence)
        .collect();
    assert!(!recovered_sequences.is_empty());
    assert_eq!(recovered_sequences[0], cursor + 1);
    assert!(
        recovered_sequences
            .windows(2)
            .all(|pair| pair[1] == pair[0] + 1)
    );
    assert!(recovered.gaps.is_empty());

    cancellation.cancel();
    acquisition.await.unwrap().unwrap();
    server.await.unwrap().unwrap();
}

async fn history_request(
    client: &mut KasinaClient<tonic::transport::Channel>,
    token: &str,
    after_sequence: u64,
) -> kasina_protocol::v1::SampleBatch {
    client
        .get_samples_since(
            authenticated_request(
                SamplesSinceRequest {
                    client: Some(client_hello("integration-test", "0")),
                    cursors: vec![StreamCursor {
                        stream: StreamKind::RespirationForce as i32,
                        after_sequence,
                    }],
                },
                token,
            )
            .unwrap(),
        )
        .await
        .unwrap()
        .into_inner()
}

#[tokio::test]
async fn server_rejects_bad_tokens_and_incompatible_protocol_versions() {
    let token = "integration-test-token";
    let driver = SimulatedDriver::default();
    let state = ServiceState::new(Duration::from_secs(30), driver.descriptor());
    let cancellation = CancellationToken::new();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(serve(
        listener,
        KasinaRpc::new(state, token),
        cancellation.child_token(),
    ));
    let mut client = KasinaClient::connect(format!("http://{address}"))
        .await
        .unwrap();

    let authentication_error = client
        .get_service_info(
            authenticated_request(client_hello("integration-test", "0"), "wrong-token").unwrap(),
        )
        .await
        .unwrap_err();
    assert_eq!(authentication_error.code(), Code::Unauthenticated);

    let version_error = client
        .get_service_info(
            authenticated_request(
                ClientHello {
                    protocol_major: 999,
                    protocol_minor: 0,
                    client_name: "integration-test".to_owned(),
                    client_version: "0".to_owned(),
                },
                token,
            )
            .unwrap(),
        )
        .await
        .unwrap_err();
    assert_eq!(version_error.code(), Code::FailedPrecondition);

    cancellation.cancel();
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn recording_rpcs_capture_live_samples_and_publish_an_analysis_ready_session() {
    let token = "integration-test-token";
    let temporary = tempfile::tempdir().unwrap();
    let recordings = temporary.path().join("sessions");
    let driver: Box<dyn SensorDriver> =
        Box::new(SimulatedDriver::with_period(Duration::from_millis(10)));
    let descriptor = driver.descriptor();
    let state = ServiceState::new_multi_with_recordings(
        Duration::from_secs(30),
        vec![descriptor],
        recordings.clone(),
    )
    .unwrap();
    let cancellation = CancellationToken::new();
    let acquisition =
        tokio::spawn(Arc::clone(&state).run_driver(driver, cancellation.child_token()));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(serve(
        listener,
        KasinaRpc::new(Arc::clone(&state), token),
        cancellation.child_token(),
    ));
    let mut client = KasinaClient::connect(format!("http://{address}"))
        .await
        .unwrap();

    let unauthenticated = client
        .start_recording(
            authenticated_request(
                StartRecordingRequest {
                    client: Some(client_hello("integration-test", "0")),
                    label: "Must not start".to_owned(),
                    notes: String::new(),
                },
                "wrong-token",
            )
            .unwrap(),
        )
        .await
        .unwrap_err();
    assert_eq!(unauthenticated.code(), Code::Unauthenticated);

    let active = client
        .start_recording(
            authenticated_request(
                StartRecordingRequest {
                    client: Some(client_hello("integration-test", "0")),
                    label: "Loopback integration".to_owned(),
                    notes: "synthetic fixture".to_owned(),
                },
                token,
            )
            .unwrap(),
        )
        .await
        .unwrap()
        .into_inner();
    assert_eq!(active.state, RecordingState::Recording as i32);
    let session_id = active.session_id.clone();
    drop(client);
    tokio::time::sleep(Duration::from_millis(120)).await;
    let mut client = KasinaClient::connect(format!("http://{address}"))
        .await
        .unwrap();
    let resumed_status = client
        .get_recording_status(
            authenticated_request(client_hello("integration-test-restart", "0"), token).unwrap(),
        )
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resumed_status.state, RecordingState::Recording as i32);
    assert_eq!(resumed_status.session_id, session_id);
    assert!(resumed_status.sample_count > 0);
    let listed_while_active = client
        .list_recordings(
            authenticated_request(client_hello("integration-test-restart", "0"), token).unwrap(),
        )
        .await
        .unwrap()
        .into_inner();
    assert_eq!(listed_while_active.sessions.len(), 1);
    assert!(listed_while_active.sessions[0].sample_count >= resumed_status.sample_count);
    let completed = client
        .stop_recording(
            authenticated_request(
                StopRecordingRequest {
                    client: Some(client_hello("integration-test", "0")),
                },
                token,
            )
            .unwrap(),
        )
        .await
        .unwrap()
        .into_inner();
    assert_eq!(completed.state, RecordingState::Completed as i32);
    assert!(completed.sample_count > 0);
    assert_eq!(completed.dropped_samples, 0);

    let listed = client
        .list_recordings(
            authenticated_request(client_hello("integration-test", "0"), token).unwrap(),
        )
        .await
        .unwrap()
        .into_inner();
    assert_eq!(listed.sessions.len(), 1);
    assert_eq!(listed.sessions[0].session_id, completed.session_id);
    let session_directory = Path::new(&completed.directory);
    assert!(session_directory.starts_with(&recordings));
    let metadata: serde_json::Value =
        serde_json::from_slice(&fs::read(session_directory.join("metadata.json")).unwrap())
            .unwrap();
    assert_eq!(metadata["state"], "completed");
    assert_eq!(metadata["notes"], "synthetic fixture");
    let samples = fs::read_to_string(session_directory.join("samples.jsonl")).unwrap();
    assert_eq!(samples.lines().count() as u64, completed.sample_count);
    assert!(samples.contains("respiration_force"));
    assert!(samples.contains("heart_rate"));
    assert!(samples.contains("rr_interval"));

    cancellation.cancel();
    acquisition.await.unwrap().unwrap();
    server.await.unwrap().unwrap();
}
