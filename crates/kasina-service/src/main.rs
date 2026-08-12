use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use kasina_devices::{SensorDriver, SimulatedDriver};
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

    let driver: Box<dyn SensorDriver> = Box::new(SimulatedDriver::default());
    let descriptor = driver.descriptor();
    let state = ServiceState::new(Duration::from_secs(args.retention_seconds), descriptor);
    let cancellation = CancellationToken::new();
    let acquisition =
        tokio::spawn(Arc::clone(&state).run_driver(driver, cancellation.child_token()));
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
    acquisition.await.context("acquisition task panicked")??;
    server_result
}
