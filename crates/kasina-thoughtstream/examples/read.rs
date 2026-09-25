//! Bounded hardware smoke test: cargo run -p kasina-thoughtstream --example read -- [PORT|auto] [SECONDS]
use std::time::Duration;

use anyhow::{Result, bail};
use kasina_devices::{DriverEvent, SensorDriver};
use kasina_thoughtstream::ThoughtStreamDriver;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let port = args.next().filter(|port| port != "auto");
    let seconds: u64 = args.next().map_or(Ok(10), |value| value.parse())?;
    let cancellation = CancellationToken::new();
    let (sender, mut receiver) = mpsc::channel(256);
    let driver =
        tokio::spawn(Box::new(ThoughtStreamDriver::new(port)).run(sender, cancellation.clone()));
    let deadline = tokio::time::sleep(Duration::from_secs(seconds));
    tokio::pin!(deadline);
    let mut counts = std::collections::BTreeMap::new();
    loop {
        tokio::select! {
            () = &mut deadline => break,
            event = receiver.recv() => match event {
                Some(DriverEvent::Measurement { stream, value, quality_flags, .. }) => {
                    *counts.entry(stream).or_insert(0_u64) += 1;
                    println!("{stream:?}: {value:.3} {} flags={quality_flags:#04x}", stream.unit());
                }
                Some(event) => println!("{event:?}"),
                None => break,
            }
        }
    }
    cancellation.cancel();
    driver.await??;
    println!("Samples in {seconds} seconds: {counts:?}");
    if counts.is_empty() {
        bail!("no valid ThoughtStream samples received");
    }
    Ok(())
}
