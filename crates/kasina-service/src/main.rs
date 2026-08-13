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
    load_or_create_token, serve, write_diagnostics_jsonl,
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
    /// Override the standard private session-recordings directory.
    #[arg(long)]
    recordings_dir: Option<PathBuf>,
    /// Acquisition source. `hardware` supervises Polar and Go Direct concurrently.
    #[arg(long, value_enum, default_value_t = Source::Simulated)]
    source: Source,
    /// Prefer this saved platform peripheral identifier for the Polar sensor.
    #[arg(long)]
    polar_id: Option<String>,
    /// Prefer this saved platform peripheral identifier for the Go Direct sensor.
    #[arg(long)]
    go_direct_id: Option<String>,
    /// Append periodic service/device/stream health snapshots as JSON Lines.
    #[arg(long)]
    diagnostics_jsonl: Option<PathBuf>,
    /// Seconds between append-only diagnostic snapshots.
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u64).range(1..))]
    diagnostics_interval_seconds: u64,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Source {
    Simulated,
    Polar,
    GoDirect,
    Hardware,
}

fn polar_driver(target_id: Option<&str>) -> PolarDriver {
    target_id.map_or_else(PolarDriver::default, PolarDriver::with_target_id)
}

fn go_direct_driver(target_id: Option<&str>) -> GoDirectDriver {
    target_id.map_or_else(GoDirectDriver::default, GoDirectDriver::with_target_id)
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
    let recordings_dir = args.recordings_dir.unwrap_or(defaults.recordings);
    let _lock = ServiceLock::acquire(&lock_path)?;
    let token = load_or_create_token(&token_path)?;

    let drivers: Vec<Box<dyn SensorDriver>> = match args.source {
        Source::Simulated => vec![Box::new(SimulatedDriver::default())],
        Source::Polar => vec![Box::new(polar_driver(args.polar_id.as_deref()))],
        Source::GoDirect => vec![Box::new(go_direct_driver(args.go_direct_id.as_deref()))],
        Source::Hardware => vec![
            Box::new(polar_driver(args.polar_id.as_deref())),
            Box::new(go_direct_driver(args.go_direct_id.as_deref())),
        ],
    };
    let descriptors = drivers
        .iter()
        .map(|driver| driver.descriptor())
        .collect::<Vec<_>>();
    let state = ServiceState::new_multi_with_recordings(
        Duration::from_secs(args.retention_seconds),
        descriptors,
        recordings_dir.clone(),
    )?;
    let cancellation = CancellationToken::new();
    let acquisitions = drivers
        .into_iter()
        .map(|driver| {
            tokio::spawn(Arc::clone(&state).run_driver(driver, cancellation.child_token()))
        })
        .collect::<Vec<_>>();
    let diagnostics_cancellation = CancellationToken::new();
    let diagnostics = args.diagnostics_jsonl.map(|path| {
        let diagnostics_state = Arc::clone(&state);
        let diagnostics_cancel = diagnostics_cancellation.child_token();
        let interval = Duration::from_secs(args.diagnostics_interval_seconds);
        tokio::spawn(async move {
            let result =
                write_diagnostics_jsonl(diagnostics_state, path, interval, diagnostics_cancel)
                    .await;
            if let Err(error) = &result {
                error!(%error, "diagnostics writer stopped");
            }
            result
        })
    });
    let listener = bind_loopback(args.port).await?;
    info!(
        address = %listener.local_addr()?,
        token_path = %token_path.display(),
        recordings_dir = %recordings_dir.display(),
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
    let mut acquisition_result = Ok(());
    for acquisition in acquisitions {
        let result = acquisition
            .await
            .context("acquisition task panicked")
            .and_then(|result| result);
        if acquisition_result.is_ok() {
            acquisition_result = result;
        }
    }
    diagnostics_cancellation.cancel();
    let diagnostics_result = if let Some(diagnostics) = diagnostics {
        diagnostics
            .await
            .context("diagnostics task panicked")
            .and_then(|result| result)
    } else {
        Ok(())
    };
    acquisition_result?;
    diagnostics_result?;
    server_result
}
