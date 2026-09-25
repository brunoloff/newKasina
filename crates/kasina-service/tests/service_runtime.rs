//! Runtime-level checks use real loopback RPC without requiring physical sensors.

use std::time::Duration;

use kasina_protocol::client_hello;
use kasina_protocol::v1::kasina_client::KasinaClient;
use kasina_protocol::v1::{
    ConnectionState, DeviceCommand, PreferredDeviceCommand, RecordingState, StartRecordingRequest,
    SubscribeRequest, ThoughtStreamPortCommand,
};
use kasina_service::{
    PreparedService, ServiceLock, ServiceOptions, ServiceSource, authenticated_request,
    port_settings, read_token,
};
use tokio_util::sync::CancellationToken;

fn options(root: &std::path::Path) -> ServiceOptions {
    ServiceOptions {
        port: 0,
        token_path: Some(root.join("service-token")),
        lock_path: Some(root.join("service.lock")),
        recordings_dir: Some(root.join("sessions")),
        device_settings_path: Some(root.join("service-devices.json")),
        source: ServiceSource::Simulated,
        ..ServiceOptions::default()
    }
}

fn hello() -> kasina_protocol::v1::ClientHello {
    client_hello("runtime-test", "0")
}

#[tokio::test]
async fn embedded_defaults_to_hardware_and_cancels_before_opening_sensors() {
    assert_eq!(ServiceOptions::default().source, ServiceSource::Hardware);
    let root = tempfile::tempdir().unwrap();
    let mut options = options(root.path());
    options.source = ServiceSource::Hardware;
    let prepared = PreparedService::prepare(options.clone()).await.unwrap();
    let state = prepared.state();
    assert_eq!(state.status_snapshot().devices.len(), 3);
    let cancel = CancellationToken::new();
    cancel.cancel();
    prepared.run(cancel).await.unwrap();
    assert!(state.status_snapshot().streams.is_empty());
    assert!(
        state
            .status_snapshot()
            .devices
            .iter()
            .all(|device| !device.connection_enabled)
    );
    drop(ServiceLock::acquire(options.lock_path.as_ref().unwrap()).unwrap());
}

#[tokio::test]
async fn singleton_and_port_are_reserved_before_recording_or_hardware_start() {
    let first_root = tempfile::tempdir().unwrap();
    let first_options = options(first_root.path());
    let first = PreparedService::prepare(first_options.clone())
        .await
        .unwrap();
    let error = PreparedService::prepare(first_options.clone())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("another kasina-service"));
    assert!(first.state().status_snapshot().streams.is_empty());

    let second_root = tempfile::tempdir().unwrap();
    let mut second_options = options(second_root.path());
    second_options.port = first.address().port();
    let error = PreparedService::prepare(second_options.clone())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("bind kasina-service"));
    assert!(!second_options.recordings_dir.as_ref().unwrap().exists());
    // A failed bind relinquishes the alternate singleton immediately.
    drop(ServiceLock::acquire(second_options.lock_path.as_ref().unwrap()).unwrap());
    drop(first);
    let restarted = PreparedService::prepare(first_options).await.unwrap();
    assert!(restarted.state().status_snapshot().streams.is_empty());
}

#[tokio::test]
async fn controls_pause_and_restart_real_acquisition_and_shutdown_finalizes_with_clients_attached()
{
    let root = tempfile::tempdir().unwrap();
    let options = options(root.path());
    let prepared = PreparedService::prepare(options.clone()).await.unwrap();
    let address = prepared.address();
    let state = prepared.state();
    let token = read_token(options.token_path.as_ref().unwrap()).unwrap();
    let cancel = CancellationToken::new();
    let runner = tokio::spawn(prepared.run(cancel.clone()));
    let mut client = KasinaClient::connect(format!("http://{address}"))
        .await
        .unwrap();
    let info = client
        .get_service_info(authenticated_request(hello(), &token).unwrap())
        .await
        .unwrap()
        .into_inner();
    assert_eq!(info.instance_id, state.instance_id());
    let mut status = client
        .subscribe_status(authenticated_request(hello(), &token).unwrap())
        .await
        .unwrap()
        .into_inner();
    let mut samples = client
        .subscribe_samples(
            authenticated_request(
                SubscribeRequest {
                    client: Some(hello()),
                    streams: vec![],
                },
                &token,
            )
            .unwrap(),
        )
        .await
        .unwrap()
        .into_inner();
    assert!(status.message().await.unwrap().is_some());
    let started = client
        .start_recording(
            authenticated_request(
                StartRecordingRequest {
                    client: Some(hello()),
                    label: "Graceful exit".to_owned(),
                    notes: String::new(),
                },
                &token,
            )
            .unwrap(),
        )
        .await
        .unwrap()
        .into_inner();
    assert!(!samples.message().await.unwrap().unwrap().samples.is_empty());

    let paused = client
        .set_device_connection(
            authenticated_request(
                DeviceCommand {
                    client: Some(hello()),
                    device_id: "simulated:biofeedback".to_owned(),
                    connect: false,
                },
                &token,
            )
            .unwrap(),
        )
        .await
        .unwrap()
        .into_inner();
    assert_eq!(paused.state, ConnectionState::Disconnected as i32);
    assert!(!paused.connection_enabled);
    assert!(paused.connection_control_available);
    let sequences = state.status_snapshot().streams;
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        sequences
            .iter()
            .map(|s| s.newest_sequence)
            .collect::<Vec<_>>(),
        state
            .status_snapshot()
            .streams
            .iter()
            .map(|s| s.newest_sequence)
            .collect::<Vec<_>>()
    );
    let enabled = client
        .set_device_connection(
            authenticated_request(
                DeviceCommand {
                    client: Some(hello()),
                    device_id: "simulated:biofeedback".to_owned(),
                    connect: true,
                },
                &token,
            )
            .unwrap(),
        )
        .await
        .unwrap()
        .into_inner();
    assert!(enabled.connection_enabled);
    tokio::time::timeout(Duration::from_secs(2), async {
        while state
            .status_snapshot()
            .streams
            .iter()
            .map(|s| s.newest_sequence)
            .collect::<Vec<_>>()
            == sequences
                .iter()
                .map(|s| s.newest_sequence)
                .collect::<Vec<_>>()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();

    assert!(ServiceLock::acquire(options.lock_path.as_ref().unwrap()).is_err());
    cancel.cancel();
    // Keep client streams and an Arc<State> alive: neither may prevent shutdown or
    // leave a recorder writing after the singleton becomes available again.
    tokio::time::timeout(Duration::from_secs(5), runner)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let completed = state.status_snapshot().recording.unwrap();
    assert_eq!(completed.state, RecordingState::Completed as i32);
    assert!(completed.sample_count > 0);
    let metadata: serde_json::Value = serde_json::from_slice(
        &std::fs::read(std::path::Path::new(&started.directory).join("metadata.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(metadata["state"], "completed");
    assert_eq!(metadata["sample_count"], completed.sample_count);
    let mut restart_options = options;
    restart_options.port = address.port();
    let replacement = PreparedService::prepare(restart_options).await.unwrap();
    assert_ne!(state.instance_id(), replacement.state().instance_id());
}

#[tokio::test]
async fn unauthenticated_or_unsupported_controls_do_not_report_success() {
    let root = tempfile::tempdir().unwrap();
    let options = options(root.path());
    let prepared = PreparedService::prepare(options.clone()).await.unwrap();
    let address = prepared.address();
    let state = prepared.state();
    let token = read_token(options.token_path.as_ref().unwrap()).unwrap();
    let cancel = CancellationToken::new();
    let runner = tokio::spawn(prepared.run(cancel.clone()));
    let mut client = KasinaClient::connect(format!("http://{address}"))
        .await
        .unwrap();
    let command = DeviceCommand {
        client: Some(hello()),
        device_id: "simulated:biofeedback".to_owned(),
        connect: false,
    };
    assert_eq!(
        client
            .set_device_connection(command.clone())
            .await
            .unwrap_err()
            .code(),
        tonic::Code::Unauthenticated
    );
    assert!(state.status_snapshot().devices[0].connection_enabled);
    let mut invalid = command.clone();
    invalid.client.as_mut().unwrap().protocol_major += 1;
    assert_eq!(
        client
            .set_device_connection(authenticated_request(invalid, &token).unwrap())
            .await
            .unwrap_err()
            .code(),
        tonic::Code::FailedPrecondition
    );
    let mut unknown = command;
    unknown.device_id = "unknown".to_owned();
    assert_eq!(
        client
            .set_device_connection(authenticated_request(unknown, &token).unwrap())
            .await
            .unwrap_err()
            .code(),
        tonic::Code::NotFound
    );
    assert_eq!(
        client
            .set_preferred_device(
                authenticated_request(
                    PreferredDeviceCommand {
                        client: Some(hello()),
                        device_id: "simulated:biofeedback".to_owned(),
                        kind: 3,
                    },
                    &token
                )
                .unwrap()
            )
            .await
            .unwrap_err()
            .code(),
        tonic::Code::Unimplemented
    );
    assert_eq!(
        client
            .list_thought_stream_ports(hello())
            .await
            .unwrap_err()
            .code(),
        tonic::Code::Unauthenticated
    );
    assert_eq!(
        client
            .set_thought_stream_port(ThoughtStreamPortCommand {
                client: Some(hello()),
                port: "COM3".to_owned()
            })
            .await
            .unwrap_err()
            .code(),
        tonic::Code::Unauthenticated
    );
    assert_eq!(
        client
            .set_thought_stream_port(
                authenticated_request(
                    ThoughtStreamPortCommand {
                        client: Some(hello()),
                        port: "COM3".to_owned()
                    },
                    &token
                )
                .unwrap()
            )
            .await
            .unwrap_err()
            .code(),
        tonic::Code::FailedPrecondition
    );
    assert!(!options.device_settings_path.as_ref().unwrap().exists());
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(5), runner)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn shared_thoughtstream_preference_saves_loads_and_keeps_old_choice_on_write_failure() {
    let root = tempfile::tempdir().unwrap();
    let mut options = options(root.path());
    options.source = ServiceSource::Thoughtstream;
    let prepared = PreparedService::prepare(options.clone()).await.unwrap();
    let state = prepared.state();
    state
        .set_thoughtstream_port(Some("test-port".to_owned()))
        .await
        .unwrap();
    assert_eq!(
        port_settings::load(options.device_settings_path.as_ref().unwrap())
            .unwrap()
            .as_deref(),
        Some("test-port")
    );
    drop(prepared);
    let reloaded = PreparedService::prepare(options.clone()).await.unwrap();
    assert_eq!(
        reloaded
            .state()
            .thoughtstream_ports()
            .await
            .unwrap()
            .selected_port,
        "test-port"
    );
    // A directory at the target simulates a write failure on every platform.
    let path = options.device_settings_path.as_ref().unwrap();
    std::fs::remove_file(path).unwrap();
    std::fs::create_dir(path).unwrap();
    assert!(
        reloaded
            .state()
            .set_thoughtstream_port(Some("different-port".to_owned()))
            .await
            .is_err()
    );
    assert_eq!(
        reloaded
            .state()
            .thoughtstream_ports()
            .await
            .unwrap()
            .selected_port,
        "test-port"
    );
    std::fs::remove_dir(path).unwrap();
    reloaded.state().set_thoughtstream_port(None).await.unwrap();
    assert_eq!(
        reloaded
            .state()
            .thoughtstream_ports()
            .await
            .unwrap()
            .selected_port,
        ""
    );
    assert!(port_settings::load(path).unwrap().is_none());
}
