# newKasina

`newKasina` is a high-performance Rust rewrite of the pyKasina biofeedback application.
It consists of a persistent sensor service and a separately restartable GPU-accelerated
desktop client.

The implementation is currently under active development. See [PLAN.md](PLAN.md) for
architecture, milestones, exit criteria, and current evidence.

## Development prerequisites

The Linux build needs a current stable Rust toolchain, pkg-config, D-Bus/BlueZ development
files, Wayland/X11 development files, GTK 3 plus AppIndicator development files, ALSA,
and a working Vulkan or OpenGL driver. Windows uses the MSVC Rust target and Windows SDK.
macOS uses the Xcode command-line tools.

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

Start the persistent service and its system-tray host in one terminal:

```sh
cargo run --release -p kasina-service
```

For both physical devices, use:

```sh
cargo run --release -p kasina-service -- --source hardware
```

The tray icon has a heart and breath glyph with independent connection lights: green is
connected, amber is connecting/reconnecting, red is disconnected/error, and gray means
the device or service is off. Its menu shows live details and recording status, and can
start/stop recording, start/stop the measurement runtime, launch the client, open the
recordings directory, or cleanly quit. On Linux panels the AppIndicator backend may open
the same menu from either mouse button. See [docs/system-tray.md](docs/system-tray.md).

Then start the client:

```sh
cargo run --release -p kasina-app
```

### KDE application launchers

After a release build, install application-menu entries and scalable icons for both the
client and the hardware sensor service:

```sh
scripts/install-desktop-launchers
```

This is a per-user installation under `~/.local/share`; it does not need `sudo`. The
**newKasina Sensor Service** entry starts the tray-hosted service with both physical
device drivers, while **newKasina** opens the client. Rerun the installer if this project
directory is moved. The tracked desktop entries and icons are under `packaging/linux`.

To test the interface without wearing or powering either device, open **Settings** and
enable **Simulation mode**. The app then displays locally generated respiration at 10 Hz
and matching heart-rate/RR samples at 1 Hz. The persistent service is left untouched, and
disabling simulation returns the UI to live device data immediately.

## Record a session

Open **Settings**, enter a label and optional notes under **Session recording**, then use
**Start recording** and **Stop recording**. A red `REC` indicator remains visible while
the persistent service records every raw sample, so closing or restarting the app does
not interrupt an active session. If the service or computer stops unexpectedly, the next
service start marks the session as interrupted and recovers every complete sample line.

Recordings use the operating system's private per-user data directory. On Linux the
default root is `~/.local/share/newkasina/sessions`; the Settings view displays the exact
session directory after recording starts. For isolated testing or a portable dataset,
override the root when starting the service:

```sh
cargo run --release -p kasina-service -- \
  --recordings-dir ./sessions
```

Each dated session directory contains human-readable `metadata.json` and append-only
`samples.jsonl` files. Raw respiration force, heart rate, and RR intervals retain source,
sequence, integer timestamps, unit, and quality flags for later breath-phase and HRV
research. See [docs/recordings.md](docs/recordings.md) for the layout, schema, recovery
rules, and analysis examples.

In-app simulation is intentionally display-only. To create a synthetic recording, run
the service with its default `--source simulated` mode and leave the app's simulation
toggle off while recording.

The service binds only to `127.0.0.1:18861`. Its per-user authentication token is created
in the operating system's standard configuration directory.

The default is a deterministic dual-stream simulator. For SSH, systemd, tests, or another
environment without a graphical desktop, retain the old terminal-only behavior with
`--headless`:

```sh
cargo run --release -p kasina-service -- --headless --source hardware
```

`--source polar` and `--source go-direct` run one hardware driver for focused testing.
After the first discovery, `--polar-id ID` and `--go-direct-id ID` prefer saved platform
peripheral identifiers while retaining advertised-name/service fallback discovery.

After a release build, run the tray directly with:

```sh
./target/release/kasina-service --source hardware
```

To expose that build as a system command, `/usr/local/bin` is the conventional location:

```sh
sudo ln -sfn /home/bruno/Crapbox/Repositories/newKasina/target/release/kasina-service \
  /usr/local/bin/kasina-service
kasina-service --source hardware
```

If `/usr/bin` is specifically required, use the same link there instead:

```sh
sudo ln -sfn /home/bruno/Crapbox/Repositories/newKasina/target/release/kasina-service \
  /usr/bin/kasina-service
```

For an eight-hour dual-device Linux soak with append-only diagnostics every ten seconds:

```sh
scripts/run-hardware-soak 8h
```

The first three arguments are duration, output path, and diagnostic interval. Any later
arguments are forwarded to the service, for example:

```sh
scripts/run-hardware-soak 8h hardware-soak.jsonl 10 \
  --polar-id POLAR_ID --go-direct-id GO_DIRECT_ID
```

The generated `hardware-soak-*.jsonl` is private (mode `0600`) and ignored by Git. Each
line retains the service instance, device state/detail/reconnect count, per-stream newest
sequence/retention/loss/age, client count, and transport lag. A shorter dry run such as
`scripts/run-hardware-soak 2m` is useful before attaching the sensors overnight.

## GPU smoke testing

The Codex sandbox used during initial development cannot see `/dev/dri`. Run the client
from a normal graphical desktop session and inspect the Diagnostics panel for the selected
wgpu backend, adapter, frame-time percentiles, and upload counters.

For a repeatable release-mode measurement, this command warms up for two seconds, records
30 seconds of the 8,000-instance visualizer, writes JSON, and exits. The fourth argument
is the display's actual active refresh rate; the optional fifth argument is the required
application performance target:

```sh
scripts/run-render-benchmark 30 render-benchmark-60hz.json 8000 60 60
scripts/run-render-benchmark 30 render-benchmark-120-display.json 8000 120 60
```

The JSON records the source revision, OS/architecture, selected adapter/backend, viewport,
scale factor, presentation mode/latency, instance count, actual display refresh, selected
performance target, achieved FPS, frame-interval and UI-CPU percentiles, wgpu callback
timing, uniform upload size, and whether the p99 budget passed. A software adapter result
is always ineligible to pass the native-GPU target. Surface recovery counters and any
full-device-loss diagnostic are included in the report.
