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

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo build --workspace --release
```

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

## GPU smoke testing

The Codex sandbox used during initial development cannot see `/dev/dri`. Run the client
from a normal graphical desktop session and inspect the Diagnostics panel for the selected
wgpu backend, adapter, frame-time percentiles, and upload counters.

