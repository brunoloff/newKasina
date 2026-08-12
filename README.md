# newKasina

`newKasina` is a high-performance Rust rewrite of the pyKasina biofeedback application.
It consists of a persistent sensor service and a separately restartable GPU-accelerated
desktop client.

The implementation is currently under active development. See [PLAN.md](PLAN.md) for
architecture, milestones, exit criteria, and current evidence.

## Development prerequisites

The Linux build needs a current stable Rust toolchain, pkg-config, D-Bus/BlueZ development
files, Wayland/X11 development files, ALSA, and a working Vulkan or OpenGL driver. Windows
uses the MSVC Rust target and Windows SDK. macOS uses the Xcode command-line tools.

No physical sensors are required for development: the service defaults to deterministic
simulated Polar and respiration data.

## Build and test

To keep downloaded Cargo metadata and crates in the ignored project-local cache, use the
included wrapper:

```sh
scripts/cargo-local fmt --all --check
scripts/cargo-local clippy --workspace --all-targets --all-features -- -D warnings
scripts/cargo-local test --workspace --all-features
scripts/cargo-local build --workspace --release
```

Plain `cargo` works normally if a shared Cargo cache is preferred.

## Run

Start the persistent service in one terminal:

```sh
cargo run --release -p kasina-service
```

Then start the client:

```sh
cargo run --release -p kasina-app
```

The service binds only to `127.0.0.1:18861`. Its per-user authentication token is created
in the operating system's standard configuration directory.

The default is a deterministic dual-stream simulator. When both physical devices are
available, select the independently reconnecting hardware drivers with:

```sh
cargo run --release -p kasina-service -- --source hardware
```

`--source polar` and `--source go-direct` run one hardware driver for focused testing.

## GPU smoke testing

The Codex sandbox used during initial development cannot see `/dev/dri`. Run the client
from a normal graphical desktop session and inspect the Diagnostics panel for the selected
wgpu backend, adapter, frame-time percentiles, and upload counters.

For a repeatable release-mode measurement, this command warms up for two seconds, records
30 seconds of the 8,000-instance visualizer, writes `render-benchmark.json`, and exits:

```sh
scripts/run-render-benchmark 30 render-benchmark-60hz.json 8000 60
scripts/run-render-benchmark 30 render-benchmark-120hz.json 8000 120
```

The JSON records the source revision, OS/architecture, selected adapter/backend, viewport,
scale factor, instance count, declared display refresh rate, frame-interval and UI-CPU
percentiles, wgpu callback timing, uniform upload size, and whether the p99 budget passed.
Use the display's externally confirmed active refresh rate as the fourth argument. A
software adapter result is always marked as ineligible to pass the native-GPU target.
Surface recovery counters and any full-device-loss diagnostic are included in the report.
