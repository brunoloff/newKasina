use std::sync::Arc;
use std::time::Duration;

use kasina_devices::{SensorDriver, SimulatedDriver};
use kasina_protocol::client_hello;
use kasina_protocol::v1::kasina_client::KasinaClient;
use kasina_protocol::v1::{ClientHello, SamplesSinceRequest, StreamCursor, StreamKind};
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
