use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use kasina_devices::{SensorDriver, SimulatedDriver};
use kasina_godirect::GoDirectDriver;
use kasina_polar::PolarDriver;
use kasina_service::{
    DEFAULT_PORT, KasinaRpc, ServiceLock, ServicePaths, ServiceState, bind_loopback,
    load_or_create_token, serve,
};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(about = "Persistent newKasina sensor acquisition service")]
struct Args {
    /// Loopback TCP port.
    #[arg(long, default_value_t = DEFAULT_PORT)]
    port: u16,
    /// In-memory retention horizon.
    #[arg(long, default_value_t = 600)]
    retention_seconds: u64,
    /// Override the standard token path.
    #[arg(long)]
    token_path: Option<PathBuf>,
    /// Override the standard singleton lock path.
    #[arg(long)]
    lock_path: Option<PathBuf>,
    /// Acquisition source. `hardware` supervises Polar and Go Direct concurrently.
    #[arg(long, value_enum, default_value_t = Source::Simulated)]
    source: Source,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Source {
    Simulated,
    Polar,
    GoDirect,
    Hardware,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let args = Args::parse();
    let defaults = ServicePaths::for_user()?;
    let token_path = args.token_path.unwrap_or(defaults.token);
    let lock_path = args.lock_path.unwrap_or(defaults.lock);
    let _lock = ServiceLock::acquire(&lock_path)?;
    let token = load_or_create_token(&token_path)?;

    let drivers: Vec<Box<dyn SensorDriver>> = match args.source {
        Source::Simulated => vec![Box::new(SimulatedDriver::default())],
        Source::Polar => vec![Box::new(PolarDriver::default())],
        Source::GoDirect => vec![Box::new(GoDirectDriver::default())],
        Source::Hardware => vec![
            Box::new(PolarDriver::default()),
            Box::new(GoDirectDriver::default()),
        ],
    };
    let descriptors = drivers
        .iter()
        .map(|driver| driver.descriptor())
        .collect::<Vec<_>>();
    let state = ServiceState::new_multi(Duration::from_secs(args.retention_seconds), descriptors);
    let cancellation = CancellationToken::new();
    let acquisitions = drivers
        .into_iter()
        .map(|driver| {
            tokio::spawn(Arc::clone(&state).run_driver(driver, cancellation.child_token()))
        })
        .collect::<Vec<_>>();
    let listener = bind_loopback(args.port).await?;
    info!(
        address = %listener.local_addr()?,
        token_path = %token_path.display(),
        "kasina-service ready"
    );

    let shutdown = cancellation.clone();
    tokio::spawn(async move {
        if let Err(error) = tokio::signal::ctrl_c().await {
            error!(%error, "failed to install Ctrl+C handler");
        }
        shutdown.cancel();
    });

    let server_result = serve(listener, KasinaRpc::new(state, token), cancellation.clone()).await;
    cancellation.cancel();
    for acquisition in acquisitions {
        acquisition.await.context("acquisition task panicked")??;
    }
    server_result
}
