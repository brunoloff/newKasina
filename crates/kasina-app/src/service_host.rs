//! Own only the service we started. Existing services always remain independent.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use kasina_protocol::{client_hello, v1::kasina_client::KasinaClient};
use kasina_service::{PreparedService, ServiceOptions};
use parking_lot::RwLock;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LaunchMode {
    Embedded,
    Separate,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Phase {
    #[default]
    Offline,
    Starting,
    Owned,
    External,
    Stopping,
    Failed,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct HostStatus {
    pub phase: Phase,
    pub instance_id: Option<String>,
    pub notice: Option<String>,
}

pub(crate) struct HostConfig {
    pub mode: LaunchMode,
    pub options: ServiceOptions,
    pub endpoint: String,
    pub token_path: PathBuf,
    pub allow_start: bool,
    pub auto_start: bool,
}

enum CommandMessage {
    Start,
    Stop,
    Observed(String),
    Quit,
}

pub(crate) struct ServiceHost {
    commands: mpsc::Sender<CommandMessage>,
    status: Arc<RwLock<HostStatus>>,
    thread: Option<JoinHandle<()>>,
    pub allow_start: bool,
}

impl ServiceHost {
    pub fn new(config: HostConfig, repaint: egui::Context) -> Result<Self> {
        let (commands, receiver) = mpsc::channel();
        let status = Arc::new(RwLock::new(HostStatus::default()));
        let thread_status = Arc::clone(&status);
        let allow_start = config.allow_start;
        let auto_start = config.auto_start;
        let thread = std::thread::Builder::new()
            .name("kasina-service-host".to_owned())
            .spawn(move || {
                if let Err(error) = host_loop(config, receiver, &thread_status, &repaint) {
                    update(
                        &thread_status,
                        &repaint,
                        Phase::Failed,
                        None,
                        Some(format!("Could not start measurements: {error:#}")),
                    );
                }
            })
            .context("create measurement service thread")?;
        let host = Self {
            commands,
            status,
            thread: Some(thread),
            allow_start,
        };
        if auto_start && allow_start {
            host.start();
        }
        Ok(host)
    }

    pub fn status(&self) -> HostStatus {
        self.status.read().clone()
    }
    pub fn start(&self) {
        let _ = self.commands.send(CommandMessage::Start);
    }
    pub fn stop(&self) {
        let _ = self.commands.send(CommandMessage::Stop);
    }
    pub fn observed(&self, instance: String) {
        let _ = self.commands.send(CommandMessage::Observed(instance));
    }

    pub fn finish(&mut self) {
        let _ = self.commands.send(CommandMessage::Quit);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for ServiceHost {
    fn drop(&mut self) {
        self.finish();
    }
}

fn update(
    status: &RwLock<HostStatus>,
    repaint: &egui::Context,
    phase: Phase,
    instance_id: Option<String>,
    notice: Option<String>,
) {
    *status.write() = HostStatus {
        phase,
        instance_id,
        notice,
    };
    repaint.request_repaint();
}

struct OwnedService {
    cancellation: CancellationToken,
    task: tokio::task::JoinHandle<Result<()>>,
}

fn host_loop(
    config: HostConfig,
    receiver: mpsc::Receiver<CommandMessage>,
    status: &RwLock<HostStatus>,
    repaint: &egui::Context,
) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let mut owned: Option<OwnedService> = None;
    let mut children: Vec<Child> = Vec::new();
    let mut startup_deadline = None;
    loop {
        if owned
            .as_ref()
            .is_some_and(|service| service.task.is_finished())
        {
            let result = runtime.block_on(owned.take().unwrap().task);
            let detail = match result {
                Ok(Ok(())) => "Measurements stopped".to_owned(),
                Ok(Err(error)) => format!("Measurements stopped: {error:#}"),
                Err(error) => format!("Measurements stopped unexpectedly: {error}"),
            };
            update(status, repaint, Phase::Failed, None, Some(detail));
        }
        children.retain_mut(|process| match process.try_wait() {
            Ok(None) => true,
            Ok(Some(exit)) => {
                if !exit.success() {
                    update(status, repaint, Phase::Failed, None, Some("The tray service could not start. Check its tray menu for details; the app will keep trying to connect.".to_owned()));
                }
                false
            }
            Err(_) => false,
        });
        if startup_deadline.is_some_and(|deadline| std::time::Instant::now() > deadline) {
            startup_deadline = None;
            if status.read().phase == Phase::Starting {
                update(status, repaint, Phase::Failed, None, Some("The tray was opened, but measurements are not available yet. Check the sensor service tray menu for its startup message, or try Start again.".to_owned()));
            }
        }
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(CommandMessage::Start) if config.allow_start && owned.is_none() => {
                update(status, repaint, Phase::Starting, None, None);
                let result = runtime.block_on(async {
                    if service_listener_present(&config.endpoint).await? {
                        let token = kasina_service::read_token(&config.token_path)
                            .context("A service is already listening, but its access token could not be read")?;
                        let info = tokio::time::timeout(Duration::from_secs(3), async {
                            let mut client = KasinaClient::connect(config.endpoint.clone()).await?;
                            Ok::<_, anyhow::Error>(client.get_service_info(
                                kasina_service::authenticated_request(client_hello("kasina-app", env!("CARGO_PKG_VERSION")), &token)?
                            ).await?.into_inner())
                        }).await.context("The existing service did not respond")??;
                        update(status, repaint, Phase::External, Some(info.instance_id), None);
                        return Ok(());
                    }
                    match config.mode {
                        LaunchMode::Embedded => {
                            let service = PreparedService::prepare(config.options.clone()).await?;
                            let instance = service.state().instance_id().to_owned();
                            let cancellation = CancellationToken::new();
                            let stop = cancellation.clone();
                            owned = Some(OwnedService { cancellation, task: tokio::spawn(service.run(stop)) });
                            update(status, repaint, Phase::Owned, Some(instance), None);
                        }
                        LaunchMode::Separate => {
                            let path = std::env::current_exe()?.with_file_name("kasina-service");
                            if !path.is_file() { bail!("The sensor service is missing. Build or install kasina-service alongside this app, then try again."); }
                            let mut command = Command::new(path);
                            command.args(["--source", "hardware"]).stdin(Stdio::null());
                            #[cfg(unix)]
                            {
                                use std::os::unix::process::CommandExt as _;
                                command.process_group(0);
                            }
                            children.push(command.spawn().context("launch sensor tray service")?);
                            startup_deadline = Some(std::time::Instant::now() + Duration::from_secs(15));
                        }
                    }
                    Ok::<(), anyhow::Error>(())
                });
                if let Err(error) = result {
                    update(
                        status,
                        repaint,
                        Phase::Failed,
                        None,
                        Some(format!("Could not start measurements: {error:#}")),
                    );
                }
            }
            Ok(CommandMessage::Stop) => {
                if let Some(service) = owned.take() {
                    update(status, repaint, Phase::Stopping, None, None);
                    service.cancellation.cancel();
                    let result = runtime.block_on(service.task);
                    match result {
                        Ok(Ok(())) => update(
                            status,
                            repaint,
                            Phase::Offline,
                            None,
                            Some(
                                "Measurements stopped. Start them again when you’re ready."
                                    .to_owned(),
                            ),
                        ),
                        result => update(
                            status,
                            repaint,
                            Phase::Failed,
                            None,
                            Some(format!("Could not finish measurement shutdown: {result:?}")),
                        ),
                    }
                }
            }
            Ok(CommandMessage::Observed(instance)) => {
                startup_deadline = None;
                if owned.is_none() {
                    update(status, repaint, Phase::External, Some(instance), None);
                }
            }
            Ok(CommandMessage::Quit) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            _ => {}
        }
    }
    if let Some(service) = owned {
        service.cancellation.cancel();
        runtime
            .block_on(service.task)
            .context("join embedded measurement service")??;
    }
    // Dropping a Child handle does not terminate the independent Linux tray process.
    Ok(())
}

async fn service_listener_present(endpoint: &str) -> Result<bool> {
    let address = endpoint
        .strip_prefix("http://127.0.0.1:")
        .context("Automatic start is only available for the local measurement service")?;
    let port: u16 = address.parse().context("invalid local service port")?;
    match tokio::time::timeout(
        Duration::from_secs(2),
        tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)),
    )
    .await
    {
        Ok(Ok(_)) => Ok(true),
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::ConnectionRefused => Ok(false),
        Ok(Err(error)) => Err(error).context("check measurement service"),
        Err(_) => bail!("The local service address did not respond; no second service was started"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait_for(host: &ServiceHost, phase: Phase) -> HostStatus {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let status = host.status();
            if status.phase == phase {
                return status;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "waiting for {phase:?}: {status:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn attaching_client_never_stops_existing_service_and_owner_can_restart() {
        let directory = tempfile::tempdir().unwrap();
        let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = reservation.local_addr().unwrap().port();
        drop(reservation);
        let options = ServiceOptions {
            port,
            source: kasina_service::ServiceSource::Simulated,
            token_path: Some(directory.path().join("token")),
            lock_path: Some(directory.path().join("lock")),
            recordings_dir: Some(directory.path().join("sessions")),
            device_settings_path: Some(directory.path().join("devices.json")),
            ..ServiceOptions::default()
        };
        let config = || HostConfig {
            mode: LaunchMode::Embedded,
            endpoint: format!("http://127.0.0.1:{port}"),
            token_path: options.token_path.clone().unwrap(),
            options: options.clone(),
            allow_start: true,
            auto_start: true,
        };
        let mut owner = ServiceHost::new(config(), egui::Context::default()).unwrap();
        let first_instance = wait_for(&owner, Phase::Owned).instance_id;
        let mut client = ServiceHost::new(config(), egui::Context::default()).unwrap();
        assert_eq!(
            wait_for(&client, Phase::External).instance_id,
            first_instance
        );
        client.stop();
        client.finish();
        assert_eq!(owner.status().phase, Phase::Owned);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        assert!(
            runtime
                .block_on(service_listener_present(&config().endpoint))
                .unwrap()
        );
        owner.stop();
        wait_for(&owner, Phase::Offline);
        owner.start();
        assert_ne!(wait_for(&owner, Phase::Owned).instance_id, first_instance);
        owner.finish();
        let reservation =
            std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).unwrap();
        drop(reservation);
        assert!(kasina_service::ServiceLock::acquire(options.lock_path.as_ref().unwrap()).is_ok());
    }
}
