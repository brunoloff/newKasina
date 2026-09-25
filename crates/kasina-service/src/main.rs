mod tray;
#[cfg(target_os = "linux")]
mod tray_control;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use kasina_service::{
    DEFAULT_PORT, PreparedService, ServiceOptions, ServiceSource as Source, port_settings,
};
use kasina_thoughtstream::ThoughtStreamPortSelection;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Clone, Parser)]
#[command(about = "Persistent newKasina sensor acquisition service")]
struct Args {
    /// Run without a system tray (for servers, tests, and soak scripts).
    #[arg(long)]
    headless: bool,
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
    /// Acquisition source. `hardware` supervises Polar, Go Direct, and ThoughtStream concurrently.
    #[arg(long, value_enum, default_value_t = Source::Simulated)]
    source: Source,
    /// Prefer this saved platform peripheral identifier for the Polar sensor.
    #[arg(long)]
    polar_id: Option<String>,
    /// Prefer this saved platform peripheral identifier for the Go Direct sensor.
    #[arg(long)]
    go_direct_id: Option<String>,
    /// ThoughtStream serial port (e.g. /dev/ttyUSB0 or COM3); otherwise discover by USB product name.
    #[arg(long)]
    thoughtstream_port: Option<String>,
    #[arg(skip)]
    thoughtstream_selection: Option<ThoughtStreamPortSelection>,
    /// Append periodic service/device/stream health snapshots as JSON Lines.
    #[arg(long)]
    diagnostics_jsonl: Option<PathBuf>,
    /// Seconds between append-only diagnostic snapshots.
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u64).range(1..))]
    diagnostics_interval_seconds: u64,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let mut args = Args::parse();
    let saved_port = port_settings::settings_path().and_then(|path| port_settings::load(&path));
    let saved_port = match saved_port {
        Ok(port) => port,
        Err(error) => {
            tracing::warn!(%error, "could not load saved ThoughtStream port; using automatic discovery");
            None
        }
    };
    args.thoughtstream_selection = Some(ThoughtStreamPortSelection::new(
        args.thoughtstream_port.clone().or(saved_port),
    ));
    if args.headless {
        run_service_thread(args, CancellationToken::new(), None)
    } else {
        tray::run(args)
    }
}

pub(crate) fn run_service_thread(
    args: Args,
    cancellation: CancellationToken,
    bridge: Option<tray::ServiceBridge>,
) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("create service Tokio runtime")?;
    runtime.block_on(run_service(args, cancellation, bridge))
}

async fn run_service(
    args: Args,
    cancellation: CancellationToken,
    bridge: Option<tray::ServiceBridge>,
) -> Result<()> {
    let prepared = PreparedService::prepare(ServiceOptions {
        port: args.port,
        retention_seconds: args.retention_seconds,
        token_path: args.token_path,
        lock_path: args.lock_path,
        recordings_dir: args.recordings_dir,
        source: args.source,
        polar_id: args.polar_id,
        go_direct_id: args.go_direct_id,
        thoughtstream_port: args.thoughtstream_port,
        thoughtstream_selection: args.thoughtstream_selection,
        diagnostics_jsonl: args.diagnostics_jsonl,
        diagnostics_interval_seconds: args.diagnostics_interval_seconds,
        ..ServiceOptions::default()
    })
    .await?;
    let state = prepared.state();
    if let Some(bridge) = &bridge {
        bridge.install(
            state.clone(),
            tokio::runtime::Handle::current(),
            prepared.recordings_dir().to_path_buf(),
        );
    }
    // Only the standalone binary owns process signals. The embedded library host
    // supplies its own cancellation token when the app window closes.
    let shutdown = cancellation.clone();
    let signal = tokio::spawn(async move {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(%error, "failed to install Ctrl+C handler");
        }
        shutdown.cancel();
    });
    let result = prepared.run(cancellation).await;
    signal.abort();
    if let Some(bridge) = &bridge {
        bridge.clear(&state);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standalone_keeps_its_simulated_default_and_accepts_hardware_mode() {
        assert_eq!(
            Args::parse_from(["kasina-service", "--headless"]).source,
            Source::Simulated
        );
        assert_eq!(
            Args::parse_from(["kasina-service", "--source", "hardware"]).source,
            Source::Hardware
        );
    }
}
