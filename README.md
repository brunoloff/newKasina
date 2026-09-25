# newKasina

**Live biofeedback, breathing visuals, and a calmer place to watch your signals.**

newKasina is a Rust rewrite of pyKasina, built around a persistent
measurement service and an independently restartable, GPU-accelerated desktop app.
Close the interface, change a visual, or reopen a panel: the service keeps acquiring
and recording your sensor data.

![ThoughtStream feedback panel with delta and resistance graphs](docs/validation/thoughtstream-shading-2026-09-16.png)

## What it does

- **Breathing visuals:** respiration-driven paper disk, mandala, vortex, and
  kaleidoscope presets, with adjustable settings.
- **ThoughtStream feedback:** equally prominent resistance and delta readings,
  colour-shaded graphs, outlier-resistant scaling, original sound cues, and a
  Space-to-mute shortcut. Timing and volume preferences are saved.
- **Raw signals and diagnostics:** inspect measurements, sample quality, device
  connections, and rendering performance.
- **Session recording:** retain raw measurements with timestamps, sequences, source
  identifiers, units, and quality flags for later analysis.
- **A desktop sensor service:** automatic discovery, reconnection, a graphical
  ThoughtStream port chooser, and a tray icon with heart, breath, and thought symbols.
- **Simulation:** explore the interface without connecting any sensors.

The project is under active development. Linux/KDE is the primary validated desktop
environment. CI also targets Windows and macOS; those targets do not establish
hardware parity. The [project plan](PLAN.md) and [validation notes](docs/validation)
record the implementation's current scope and evidence.

## Supported sensors

| Device | Connection | Measurements |
| --- | --- | --- |
| Polar H10 | Bluetooth LE | Heart rate and RR intervals |
| Vernier Go Direct Respiration Belt | Bluetooth LE | Respiration force |
| MindPlace ThoughtStream USB | USB serial | Skin resistance and raw ADC counts |

ThoughtStream uses the protocol from Bruno's original pyThoughtstream application.
Its port is discovered automatically when possible; the tray menu offers a saved
manual selection. See the [ThoughtStream setup guide](docs/hardware/thoughtstream-protocol.md)
for protocol details and Linux serial permissions.

## Build and open the desktop apps

You need Rust **1.96 or newer** and a working Vulkan or OpenGL driver. On Linux,
install development dependencies for ALSA, D-Bus/BlueZ, udev, Wayland/X11, GTK 3,
and AppIndicator. Windows builds need the MSVC toolchain and Windows SDK; macOS
builds need the Xcode command-line tools.

Build both programs:

```sh
scripts/cargo-local build --workspace --release
```

The wrapper keeps downloaded dependencies in this project's ignored `.cargo-local`
cache. Plain `cargo` works too.

On Linux/KDE, install application-menu entries and icons:

```sh
scripts/install-desktop-launchers
```

The installer requires `desktop-file-validate` from `desktop-file-utils`, writes to
your user application directory, and does not require `sudo`. Then open:

1. **newKasina Sensor Service** to start acquiring from physical devices.
2. **newKasina** to open the interface.

The launchers use this checkout's release binaries. Rebuild after changing the code,
then reopen the relevant program. Rerun the installer if you move the checkout.

For a sensor-free tour, open **Settings → Simulation mode**. This generates data
inside the app and leaves the measurement service untouched. Settings also controls
which panels appear in the sidebar.

### Connecting devices

Turn on your Bluetooth sensors and connect ThoughtStream by USB. The service retries
connections automatically. Its tray symbols are vivid when connected and grayscale
otherwise. For ThoughtStream, **Choose ThoughtStream port…** lets you select a port
or return to **Find automatically**; the selection is remembered.

On Linux, a serial-port permission change may require a reboot if the desktop's
background app launcher retains its old group membership. The
[setup guide](docs/hardware/thoughtstream-protocol.md) covers this without needing
port arguments in the normal desktop workflow.

### Recording

Use **Settings → Session recording** or the service's tray menu to start and stop a
session. Closing the app does not stop an active recording. On Linux, sessions live
under `~/.local/share/newkasina/sessions`, with `metadata.json` and append-only
`samples.jsonl` files. Interrupted recordings are recovered on service startup.

In-app simulation is display-only. To record synthetic samples, run the service in
its simulated mode instead. See [recording format and recovery](docs/recordings.md).

## Command-line and headless use

From the project directory, run these in separate terminals:

```sh
# Simulated acquisition with a tray icon (the service default).
scripts/cargo-local run --release -p kasina-service

# Desktop client.
scripts/cargo-local run --release -p kasina-app
```

For real sensors without the tray interface:

```sh
scripts/cargo-local run --release -p kasina-service -- --headless --source hardware
```

Focused acquisition modes are `polar`, `go-direct`, and `thoughtstream`.
`--thoughtstream-port PORT` overrides serial selection; `--recordings-dir PATH`
selects a recording directory. See each program's `--help` for the full options.
The service listens on `127.0.0.1:18861` and uses a per-user authentication token.

## How the project fits together

```text
Polar H10 · Go Direct · ThoughtStream · Simulator
                         │
                  kasina-service
          acquisition · retention · recording
                         │
                authenticated local gRPC
                         │
                    kasina-app
        panels · breathing visuals · sound feedback
```

| Location | Responsibility |
| --- | --- |
| `crates/kasina-app` | Native egui desktop interface and saved preferences |
| `crates/kasina-service` | Persistent runtime, tray controls, recordings, and RPC server |
| `crates/kasina-polar`, `kasina-godirect`, `kasina-thoughtstream` | Device drivers |
| `crates/kasina-devices`, `kasina-domain` | Shared driver abstractions, simulator, and sample types |
| `crates/kasina-protocol`, `proto/` | Protocol generation and gRPC schema |
| `crates/kasina-render`, `kasina-audio` | GPU visuals and sound playback |
| `crates/kasina-analysis` | Analysis foundations |
| `packaging/`, `scripts/` | Desktop integration, builds, and diagnostic helpers |
| `docs/`, `fixtures/` | Design decisions, validation evidence, and replay data |

## Development checks

```sh
scripts/cargo-local fmt --all --check
scripts/cargo-local clippy --workspace --all-targets --all-features -- -D warnings
scripts/cargo-local test --workspace --all-features
scripts/cargo-local build --workspace --release
```

Normal tests do not require physical sensors. Tests that explicitly play audio are
opt-in. Build outputs, dependency/toolchain caches, local recordings, authentication
tokens, and ad hoc diagnostic logs are excluded from Git; curated validation reports
and screenshots live in `docs/validation`.

For long device tests and repeatable rendering measurements, see the
[developer diagnostics guide](docs/development.md).

## Further reading

- [ThoughtStream panel and sound controls](docs/thoughtstream-panel.md)
- [System tray and desktop service](docs/system-tray.md)
- [Recording schema and recovery](docs/recordings.md)
- [Process and IPC architecture](docs/adr/0001-process-and-ipc.md)
- [Rendering architecture](docs/adr/0002-rendering.md)
- [Visuals and presets](docs/adr/0003-kasina-visuals-and-presets.md)
- [Polar H10 hardware validation](docs/hardware/polar-h10-smoke.md)
- [Go Direct protocol audit](docs/hardware/godirect-protocol-audit.md)
- [Implementation plan](PLAN.md)
