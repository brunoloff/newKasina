# newKasina implementation plan

Last updated: 2026-08-13

## Progress

- [x] Architecture and implementation plan written.
- [x] Milestone 0 — repository and quality baseline.
- [x] Milestone 1 — simulated end-to-end vertical slice.
- [ ] Milestone 2 — rendering proof.
- [ ] Milestone 3 — hardware feasibility and acquisition.
- [ ] Milestone 4 — analysis parity.
- [ ] Milestone 5 — interaction, audio, and recording.
- [ ] Milestone 6 — packaging and hardening.

Update this ledger and add brief dated evidence beneath a milestone as work is completed.
Do not mark a milestone complete until its exit criteria pass.

### Evidence — 2026-08-12 to 2026-08-13

- Milestone 0: the workspace passes `cargo fmt --all --check`, warning-free Clippy,
  all-feature workspace tests, and an optimized release build. CI contains Linux,
  Windows, and macOS jobs. The project-local Cargo and rustup caches are ignored.
- Milestone 1: an authenticated real tonic server/client test proves that a client
  restart preserves service sequence numbers and fills retained history without
  duplicates. Forced service-broadcast and UI-queue pressure tests expose drops. A
  release Xvfb/llvmpipe smoke test restarted the real UI while the service instance
  remained unchanged and its recoverable history grew from 180 to 1,183 samples.
- Milestone 2 partial: the release client visibly rendered 8,000 instanced particles via
  `egui_wgpu::CallbackTrait`; shader parsing, 32-byte uniform uploads, resize, unmap/map,
  and F11 handling were exercised without validation errors. The sandbox exposes no
  `/dev/dri`, so native GPU 60/120 Hz measurements, real-window-manager fullscreen, and
  mixed-DPI validation were initially unavailable inside Codex. Software llvmpipe timing
  is recorded only as a functional smoke result. A subsequent normal KDE Wayland run on
  the physical Intel Meteor Lake Arc GPU delivered 3,594 frames in 30 seconds at the
  panel's active 120 Hz mode: 8.346 ms average, 9.130 ms p95, and 9.583 ms p99, with
  0.213 ms UI-CPU p99, 0.0105 ms callback p99, a 32-byte upload, and no surface/device
  errors. This misses the optional 8.333 ms 120 Hz stretch threshold but clears the user-
  selected 16.667 ms 60 Hz requirement. The run exposed missing Wayland viewport fields;
  schema 2 fixes that and separates display rate from performance target, so a current-
  revision native rerun plus real-window-manager interaction checks remain. The app now
  has a self-terminating release benchmark that records source revision, adapter/backend,
  viewport/DPI, declared refresh, frame-interval/UI-CPU/callback percentiles, upload size,
  and a conservative hardware-only target result. Continuous animation is explicitly
  disabled for hidden/minimized viewports and inactive views are event-driven. Surface
  outdated/lost/occluded events have explicit recovery actions and visible counters; full
  device loss is captured as an actionable diagnostic. A targeted CPU preparation
  benchmark processed 20 million frames at 3.049 ns/frame on this host; work and upload
  size remain constant as instance count grows.
- Milestone 3 hardware-independent work: Polar HR/RR parsing covers 8/16-bit heart rate,
  energy, contact, multiple RR intervals, and malformed frames. Both native `btleplug`
  drivers discover across every adapter, prefer optional saved platform IDs, bound BLE
  operations and notification silence, clean up on cancellation, and retry with capped
  jittered exponential backoff that resets only after successful session setup. Go Direct
  command/response framing, fragmentation/back-to-back packets, identity/status/channel
  metadata, period validation, and every documented value layout are tested against
  Vernier's reference implementations. The service supervises both drivers concurrently,
  exposes structured connection state, and can retain private append-only health JSONL.
  A twelve-second hardware-mode dry run completed two full discovery/retry cycles per
  unavailable sensor and produced seven snapshots including a clean final disconnected
  state. The 36-test workspace and release build pass. Physical comparison, power-cycle
  recovery, and the real eight-hour soak remain required.
- Milestone 3 physical Go Direct smoke, 2026-08-13: the native driver discovered and
  connected to a powered GDX-RB through BlueZ, completed initialization and every metadata
  query, selected channel 1 (`Force`, `N`) at 100 ms, and streamed 1,198 sequential samples
  across 119.683 seconds (effectively 10.0 Hz). Diagnostics showed connected state, 49 ms
  sample age, no reconnect, no retention loss, and no transport lag. The physical
  initialization response proved that command payload byte zero is command-specific
  rather than a generic status; the
  driver was corrected to match both official Vernier implementations. Python side-by-
  side comparison and the eight-hour dual-device soak remain. A subsequent physical
  power-cycle stopped notifications, triggered the five-second silence detector, entered
  one supervised reconnect, rediscovered and fully configured the same belt, and resumed
  at 10 Hz about 12 seconds after the last pre-cycle sample. The service sequence advanced
  from 14,361 to 14,366 rather than resetting, with no transport lag.
- Milestone 3 physical dual-device smoke, 2026-08-13: hardware mode discovered a worn
  Polar H10 and the saved Go Direct belt concurrently. The H10 connected on its first
  attempt, subscribed to Heart Rate Measurement, and produced both heart-rate and RR-
  interval streams. At 44 seconds the service had retained 24 samples from each Polar
  stream and 239 respiration samples; both devices were connected, sample ages were below
  200 ms, and transport lag was zero. The belt needed one supervised retry after an
  initial command timeout, then completed setup and streamed normally. Longer comparison,
  power-cycle, client-restart, and eight-hour dual-device tests remain.
- Breath Kasina slice, 2026-08-13: the client now opens on a dedicated breath-visualization
  tab driven directly by respiration force, without depending on the legacy analysis.
  An incremental, self-calibrating force envelope maps rising force to expansion and
  falling force to contraction, with short time-based smoothing between 10 Hz device
  samples. A retained wgpu shader draws the animated circular mandala as one analytic
  full-screen instance with the existing fixed 32-byte uniform upload. Focused normalizer,
  animation, uniform-layout, and shader-parse tests pass; a release client connected to
  the live Go Direct service without transport lag. Visual tuning remains intentionally
  open to hands-on feedback.

## 1. Objective

Rebuild the existing `pyKasina` desktop biofeedback application as a high-performance,
cross-platform Rust application for Linux, Windows, and macOS.

The system must:

- acquire Polar H10 heart-rate/RR data and Vernier Go Direct respiration-belt data;
- keep device connections alive when the graphical client restarts;
- render fluid, GPU-accelerated feedback at a required 60 Hz, while retaining 120 Hz as
  a measured stretch target on displays that support it;
- preserve and then improve the useful analysis and training behavior in `pyKasina`;
- be testable without physical hardware through simulation and replay;
- expose connection health, dropped samples, timing, and rendering performance instead
  of failing silently.

The selected stack is Rust, `btleplug`, `wgpu`, and `egui`/`eframe`.

## 2. Scope and non-goals

### Initial supported platforms

- Linux with BlueZ, initially developed and hardware-tested on Manjaro.
- Windows 10/11 using the MSVC Rust target.
- macOS 10.15 or later.

### Initial device scope

- Polar H10 over Bluetooth Low Energy.
- The existing Vernier Go Direct respiration belt over Bluetooth Low Energy.
- Go Direct USB support is desirable, but is a later milestone unless BLE proves
  unreliable.
- Polar accelerometer and ECG support should fit the architecture, but HR and RR are
  the first required streams.

### Non-goals for the first complete version

- Android, iOS, and browser deployment.
- Medical-device certification or diagnostic claims.
- Cloud accounts or mandatory network services.
- Bit-for-bit reproduction of accidental behavior or bugs in the Python application.

## 3. High-level architecture

The application consists of two independently restartable processes:

```text
Polar H10 ───────┐
                 ├── kasina-service ── versioned localhost stream ── kasina-app
Go Direct belt ──┘         │                                      ├── egui UI
                           ├── bounded memory buffers              ├── analysis workers
                           ├── optional session recording          ├── audio feedback
                           └── reconnect state machines            └── custom wgpu renderer
```

`kasina-service` is a per-user background process, not a privileged system daemon.
It owns Bluetooth, reconnects devices, assigns sequence numbers and timestamps, retains
recent samples, and optionally records sessions. Closing or rebuilding `kasina-app` must
not affect sensor connections.

`kasina-app` connects to the service, requests a recent snapshot, subscribes to live
samples, runs deterministic analysis pipelines, produces audio feedback, and renders the
interface. On an IPC interruption it reconnects and fills any recoverable sequence gap
from the service buffer.

## 4. Proposed Cargo workspace

```text
newKasina/
├── Cargo.toml
├── Cargo.lock
├── PLAN.md
├── README.md
├── crates/
│   ├── kasina-domain/       # sample types, time model, ring buffers, errors
│   ├── kasina-analysis/     # filtering, breath phases, RSA, RMSSD, streaks
│   ├── kasina-protocol/     # protobuf schema and generated IPC types
│   ├── kasina-devices/      # SensorDriver trait and simulated/replay drivers
│   ├── kasina-polar/        # Polar H10 btleplug implementation
│   ├── kasina-godirect/     # Go Direct protocol and btleplug implementation
│   ├── kasina-service/      # persistent acquisition service and RPC server
│   ├── kasina-render/       # custom wgpu pipelines and retained GPU resources
│   ├── kasina-audio/        # tones, mixer, and later speech
│   └── kasina-app/          # eframe/egui desktop client
├── proto/
│   └── kasina.proto
├── fixtures/                # small, consent-safe raw/replay and protocol fixtures
├── packaging/
│   ├── linux/
│   ├── macos/
│   └── windows/
└── scripts/                 # developer and packaging helpers only
```

Crates may initially be combined when that keeps the first vertical slice small. The
dependency direction must remain acyclic: domain at the bottom; drivers, service,
analysis, audio, and renderer above it; the app at the top.

## 5. Core technical decisions

### 5.1 UI and rendering

- Start with `eframe` using its default `wgpu` renderer.
- Use the `wgpu` types re-exported by `eframe`/`egui-wgpu` to prevent incompatible
  duplicate `wgpu` versions.
- Build ordinary controls, layout, text, menus, tooltips, and low-density diagnostic
  views with egui.
- Render the central live biofeedback canvas through
  `egui_wgpu::CallbackTrait`. The callback receives the active wgpu render pass and
  composes custom output directly inside the egui region.
- Keep GPU pipelines, buffers, bind groups, and textures retained across frames.
  Upload only changed sample ranges and uniforms.
- Batch repeated marks and shapes with instancing. Use WGSL shaders for effects that
  would otherwise create large CPU-generated meshes.
- Use logical points for egui layout and physical pixels for render targets. Test mixed
  DPI and live DPI changes.
- Let vsync control active animation. Request continuous repaints only while a visualizer
  is visible or animating; use event-driven repaints when idle or minimized.
- Begin with one window. Do not move to a hand-written `winit` event loop until an
  identified eframe limitation requires it.

The first renderer should contain a deliberately stressful synthetic scene so GPU and
CPU frame budgets can be measured before the full UI is ported.

### 5.2 Device service and IPC

- Run one Tokio runtime in `kasina-service`.
- Use a versioned protobuf contract over a loopback-only streaming RPC server. Tonic is
  the initial implementation choice because it provides generated types, streaming,
  cancellation, and explicit compatibility boundaries on all three platforms.
- Use a vendored `protoc` build dependency so contributors do not need a system-wide
  Protocol Buffers compiler merely to build the workspace.
- Bind only to `127.0.0.1`. Make the port configurable, with `18861` as a familiar
  development default.
- Generate a per-user authentication token on first launch, store it in the platform
  configuration directory with user-only permissions, and require it for data and
  device-control calls. Local biometric data should not be exposed to unrelated users.
- Include protocol major/minor versions and service build information in the handshake.
- Keep control messages separate from high-rate sample batches.
- Batch live samples for transport over a short configurable interval rather than
  sending one RPC message per sample.

Minimum RPC surface:

- `Health` and `GetServiceInfo`;
- `ListAdapters` and `ListDevices`;
- `ConnectDevice`, `DisconnectDevice`, and `SetPreferredDevice`;
- `GetDeviceStatus` and `SubscribeStatus`;
- `GetSamplesSince(stream, sequence)`;
- `SubscribeSamples(streams)`;
- `StartRecording`, `StopRecording`, and `GetRecordingStatus`;
- `Shutdown`, guarded and disabled by default so closing the UI cannot stop acquisition.

The app networking task runs off the UI thread and publishes immutable snapshots or
bounded messages to the UI. The UI thread must never block on Bluetooth, disk, IPC, or
analysis.

### 5.3 Process lifetime

- Enforce a single service instance per user with a lock and an RPC health check.
- During development the app may start the service if it is absent, but the spawned
  process must be detached from the UI lifetime.
- Production startup integration:
  - Linux: a `systemd --user` unit and optional desktop autostart entry;
  - macOS: a per-user LaunchAgent;
  - Windows: a per-user startup task/process. Prefer this over a privileged Windows
    Service because Bluetooth and user configuration naturally belong to the user
    session.
- If the UI and service versions are incompatible, show an actionable error rather than
  attempting a partial connection.

### 5.4 Data and time model

Every sample contains:

- stream kind and source device ID;
- monotonically increasing per-stream sequence number;
- service receive time on a monotonic clock;
- corresponding wall-clock timestamp for saved sessions;
- optional device timestamp;
- value and unit;
- quality/validity flags.

Required stream types:

- `HeartRate`: BPM plus optional energy value;
- `RrInterval`: one event per RR value in seconds or integer microseconds internally;
- `RespirationForce`: calibrated value and unit from the selected Go Direct channel;
- `Acceleration`: optional x/y/z samples;
- status/diagnostic events.

Do not use floating-point timestamps as identity. Use integer nanoseconds or microseconds
and convert to floating point only for plotting. Record discontinuities explicitly.

The service holds configurable time-bounded ring buffers, initially ten minutes. A
request older than the retained range returns the available data plus an explicit gap
marker. It must never silently pretend that a sequence is continuous.

### 5.5 Storage and recordings

- Keep live buffering in memory.
- Add optional raw-session recording after the acquisition vertical slice works.
- Start with SQLite in WAL mode or another crash-tolerant append-oriented store. The
  modest sensor rates do not justify a bespoke binary format.
- Store raw samples, device metadata, calibration/settings snapshots, stream gaps, app
  and service versions, and session annotations.
- Derived metrics should generally be recomputable from raw data. Persist them only as
  cached results with algorithm/version metadata.
- Provide export to CSV after the native session format is stable.

### 5.6 Configuration and observability

- Use platform user config/data directories through the `directories` crate.
- Store human-editable configuration as versioned TOML.
- Use `tracing` for structured logs in both processes, with rotating log files.
- Surface connection state, reconnect attempts, sample rate, last sample age, sequence
  gaps, dropped messages, ring-buffer duration, renderer backend/adapter, FPS, average,
  p95 and p99 frame time in a Diagnostics view.
- Install panic hooks that preserve useful crash information without swallowing errors.
- Never log raw biometric streams by default.

## 6. Hardware integration

### 6.1 Polar H10

Use `btleplug` directly for the required HR/RR path.

Implementation outline:

1. Scan without relying solely on a BlueZ UUID filter; Linux merges scan filters from
   multiple D-Bus clients, so post-filter advertisements in the application.
2. Select by saved platform device identifier, advertised Polar name, and Heart Rate
   service UUID.
3. Connect and discover services.
4. Subscribe to Heart Rate Measurement characteristic `0x2A37` in service `0x180D`.
5. Parse every flag combination: 8/16-bit HR, energy present/absent, and zero or multiple
   RR values in a single notification.
6. Convert RR units of 1/1024 second without premature rounding.
7. Monitor notification age and disconnect events.
8. Reconnect with cancellable exponential backoff and jitter, capped at 30 seconds.
9. Expose battery and device information later without coupling them to the sample path.

Unit tests must cover captured or specification-derived notification byte sequences,
including malformed and truncated frames.

### 6.2 Go Direct respiration belt

This is the largest technical risk because Vernier publishes Python and JavaScript
libraries, but no supported Rust library.

Treat it as an early protocol spike:

1. Identify the exact belt model, advertised name, GATT services, characteristics,
   sensor-channel metadata, units, and notification cadence.
2. Audit Vernier's BSD-licensed `godirect-js` and `godirect-py` implementations and
   document the subset of commands used for:
   - discovery and connection;
   - device/sensor information;
   - channel selection;
   - starting a 100 ms measurement period;
   - parsing streamed measurement packets;
   - stopping and clean disconnect.
3. Capture consent-safe raw GATT request/response fixtures with the existing Python
   service and the same physical belt.
4. Implement protocol encoding/decoding as pure Rust functions before adding BLE I/O.
5. Connect those functions to a `btleplug` driver and verify values side-by-side against
   `pyKasina`/`godirect` for at least 30 minutes.
6. Validate reconnect behavior and power cycling.

Keep an optional, clearly isolated Python Go Direct bridge as a temporary development
fallback. It may unblock UI and service work, but it is not the target architecture and
must not become a hidden production dependency.

USB/HID support can later be added behind the same `SensorDriver` trait, probably using
`hidapi`, without changing the service protocol or UI.

### 6.3 Driver abstraction

Drivers implement a small async interface and publish domain events rather than leaking
`btleplug` types:

```text
SensorDriver
  discover(adapter) -> candidates
  connect(candidate, cancellation)
  capabilities()
  start(stream request, event sender)
  stop()
  disconnect()
```

Provide `SimulatedPolar`, `SimulatedRespiration`, and `ReplayDriver` implementations.
All application development and CI must work with no Bluetooth adapter and no sensors.

## 7. Analysis migration

Place analysis in `kasina-analysis` as deterministic, UI-independent state machines.
Inputs and outputs should be plain domain types. Analysis must be incremental: adding a
sample should not repeatedly filter or scan the full ten-minute history.

Initial parity work:

- configurable low-pass filtering of respiration force;
- respiration direction and phase detection: inhale, hold, exhale;
- phase lengths and breaths per minute;
- alignment of RR events with respiration phases;
- RR extrema and respiratory sinus arrhythmia amplitude;
- RMSSD over a configurable number of completed breath cycles;
- HRV streak calculation;
- histogram/current-percentile calculations used by the trainer.

Before changing algorithms, encode the intended Python behavior in golden tests using
synthetic traces and small recorded fixtures. Then improve known weaknesses explicitly:

- use timestamp-aware filtering rather than assuming perfectly uniform samples;
- add hysteresis/debounce to phase transitions;
- distinguish sensor gaps from genuine breath holds;
- avoid tie-breaking by perturbing measurements;
- report confidence and insufficient-data states instead of indexing short arrays;
- give every metric an algorithm/version identifier.

## 8. Desktop interface

The initial application views are:

1. **Dashboard** — device state, current HR/RR, respiration phase, session controls.
2. **Raw** — live respiration, HR/RR, optional accelerometer, timestamps and gaps.
3. **Filtered** — raw/filtered respiration, phase classification, tuning controls.
4. **Pranayama trainer** — phase duration, breath rate, aligned RSA feedback, audio
   controls, and a dedicated GPU visual canvas.
5. **HRV trainer** — RR curve, extrema, RSA, streak, configurable cycle RMSSD.
6. **RR geometry** — port and refine the experimental arc/square visualization.
7. **Settings** — preferred devices, reconnect policy, buffers, audio, rendering.
8. **Diagnostics** — service logs/status, sample counters, frame timings, GPU backend.

Use a coherent application-wide state model. Separate:

- service connection state;
- immutable raw-data snapshots;
- analysis-worker state;
- user settings;
- short-lived UI state;
- renderer resources.

Do not clone full sample histories each frame. UI plots consume compact snapshots or
GPU-ready ranges with stable ownership.

## 9. Audio

- Port synthesized heartbeat tones to a native Rust mixer using `cpal` or a thin layer
  built on it.
- Precompute tone envelopes and reuse buffers; the real-time audio callback must not
  allocate, lock, perform I/O, or log.
- Send timestamped audio commands through a bounded queue and expose underrun/late-event
  counters.
- Preserve distinct pitch/double-beep feedback only after its training semantics are
  covered by tests.
- Treat offline speech as a separate milestone. Prefer a bundled, deterministic local
  voice implementation or a documented Piper subprocess integration. Do not block the
  core application on cross-platform TTS.

## 10. Implementation milestones

### Milestone 0 — repository and quality baseline

- Initialize the Cargo workspace and Git metadata correctly.
- Add formatting, Clippy, test, and dependency-policy commands.
- Add CI for native Linux, Windows, and macOS builds.
- Add a short developer README and architecture decision records for IPC and rendering.

Exit criteria:

- `cargo fmt --all --check` passes;
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` passes;
- `cargo test --workspace --all-features` passes on the scaffold;
- CI contains all three desktop operating systems.

### Milestone 1 — simulated end-to-end vertical slice

- Define domain samples, sequences, statuses, and bounded buffers.
- Implement simulated Polar and respiration streams.
- Start `kasina-service`, connect `kasina-app`, fetch history, and stream live batches.
- Display connection state and raw signals in egui.
- Restart the UI repeatedly while the service sequence continues uninterrupted.

Exit criteria:

- a UI restart does not restart the service or reset sequence numbers;
- reconnect fills the retained gap without duplicates;
- backpressure and intentional drops appear in diagnostics;
- the entire slice runs without Bluetooth or GPU-specific test hardware.

### Milestone 2 — rendering proof

- Integrate a custom `egui-wgpu` paint callback.
- Render a stress scene plus a first biofeedback visual from simulated data.
- Add frame-time and upload-size instrumentation.
- Verify device loss/surface resize handling, fullscreen, DPI changes, and minimizing.

Exit criteria:

- release-mode frame pacing meets the targets in section 12 on the development machine;
- UI controls remain responsive while the visualizer runs;
- inactive views do not continuously consume a full core.

### Milestone 3 — hardware feasibility and acquisition

- Complete the Polar H10 driver and parser tests.
- Complete the Go Direct protocol spike and choose native port versus temporary bridge.
- Implement both reconnecting supervisors.
- Run side-by-side measurement comparisons with the Python server.

Exit criteria:

- Polar HR and every RR interval match parsed reference data;
- Go Direct values, units, channel, and rate match the Python implementation;
- device power cycling recovers automatically;
- the service survives an eight-hour hardware soak with diagnostics retained.

### Milestone 4 — analysis parity

- Port the analysis functions into incremental state machines.
- Add golden, boundary, gap, and property tests.
- Build Raw, Filtered, HRV, and Pranayama views against replay data.

Exit criteria:

- metrics agree with approved Python fixtures within documented tolerances;
- empty, short, constant, noisy, and gapped data never panic;
- analysis cost remains bounded as session duration grows.

### Milestone 5 — interaction, audio, and recording

- Add session lifecycle and crash-tolerant raw recording.
- Add heartbeat audio and timing diagnostics.
- Add configuration persistence and device selection.
- Restore the useful timer, silence, and feedback controls.

Exit criteria:

- recorded sessions replay identically through the analysis pipeline;
- audio callbacks remain allocation-free and underruns are measurable;
- corrupt/incomplete session tails recover safely.

### Milestone 6 — packaging and hardening

- Package the service, app, assets, autostart definition, and uninstaller per platform.
- Add upgrade/migration behavior for config, schema, and service protocol.
- Run native CI and manual hardware smoke tests on all target platforms.
- Complete license inventory and user-facing privacy/data-location documentation.

Exit criteria:

- install, first-run permissions, autostart, UI restart, upgrade, and uninstall work on
  Linux, Windows, and macOS;
- the app reports a useful error when Bluetooth or a GPU backend is unavailable;
- release artifacts are reproducible enough to identify their exact source revision.

## 11. Verification strategy

### Automated tests

- Pure parser tests for Polar and Go Direct packets.
- Ring-buffer sequence and retention property tests.
- IPC compatibility, authentication, snapshot, reconnect, and cancellation tests.
- Analysis golden tests and property tests for finite outputs and no panics.
- Replay determinism tests.
- egui state tests with `egui_kittest` where practical.
- Renderer unit tests for buffer preparation and optional offscreen image comparisons on
  GPU-enabled CI.
- Storage migration and interrupted-write tests.

### Benchmarks

- Incremental respiration filtering and phase detection.
- RR extrema/RMSSD updates.
- IPC encoding/decoding and sample batching.
- GPU buffer preparation and bytes uploaded per frame.
- End-to-end replay at 1x, 10x, and burst rates.

### Hardware/manual tests

- Cold discovery, saved-device reconnect, power-cycle reconnect, and out-of-range return.
- Multiple RR values in one Polar notification.
- Go Direct start/stop at 100 ms and at supported neighboring rates.
- UI restart loop while both devices remain attached to the service.
- Bluetooth adapter disable/enable.
- suspend/resume and login/logout behavior.
- audio device loss and default-device change.
- fullscreen, high DPI, multiple monitors, and 60 Hz displays; measure 120 Hz as a stretch
  target where supported.

### Required validation order for changes

1. `cargo fmt --all --check`
2. `cargo clippy --workspace --all-targets --all-features -- -D warnings`
3. `cargo test --workspace --all-features`
4. targeted benchmarks or replay tests for affected hot paths
5. release build
6. native visual/hardware smoke test when the change touches rendering or devices

## 12. Initial performance and reliability budgets

These are engineering targets, not claims about sensor accuracy:

- Required 60 Hz target: p99 application frame interval below 16.67 ms.
- Stretch 120 Hz target: p99 application frame interval below 8.33 ms; failure does not
  block the initial desktop release when the required 60 Hz target passes.
- No full-history copy, filter, extrema scan, or GPU rebuild on each frame.
- UI thread performs no blocking I/O.
- New service samples become visible within 50 ms p99 after service receipt under normal
  local load, excluding the devices' own notification cadence.
- No unreported sample loss. Every loss or retention gap increments a visible counter.
- UI may be restarted 100 times without a device reconnect caused by the UI.
- Service completes an eight-hour dual-device soak without unbounded memory growth.
- Idle/minimized UI is event-driven and does not continuously render.

Performance claims must be supported by release-mode measurements and recorded with the
GPU adapter, backend, resolution, refresh rate, OS, build revision, and data workload.

## 13. Platform prerequisites

The current Manjaro development machine already has the required initial dependencies:
Rust/Cargo, Clang/CMake/Ninja, pkg-config, D-Bus/BlueZ, ALSA, Wayland/X11 development
libraries, Vulkan headers/loader, and a powered Bluetooth adapter. No root installation
is currently required.

The Codex execution sandbox does not expose `/dev/dri`, so actual wgpu rendering cannot
be validated inside that sandbox. Run GPU smoke tests from the normal desktop session.

Likely prerequisites for other machines:

- Arch/Manjaro: Rustup plus `base-devel`, `pkgconf`, `dbus`, `bluez`, `bluez-utils`,
  `alsa-lib`, `libxkbcommon`, Wayland/X11 libraries, `vulkan-icd-loader`, and the correct
  GPU Vulkan driver such as `vulkan-intel`.
- Debian/Ubuntu: `build-essential`, `pkg-config`, `libdbus-1-dev`, `libudev-dev`,
  `libasound2-dev`, `libxkbcommon-dev`, Wayland/X11 development packages, and the
  appropriate Mesa/Vulkan driver.
- Windows: Rustup's stable MSVC toolchain, Visual Studio Build Tools with Desktop C++
  workload, and a current Windows SDK.
- macOS: current Xcode command-line tools. The application bundle must include a clear
  Bluetooth usage description and be code-signed for distribution.

Do not install packages preemptively. Add a dependency only after a real build error or
feature requires it, and document the exact package and purpose.

## 14. Risks and mitigations

### Go Direct protocol uncertainty

Mitigation: make it the first hardware spike, port only the required belt subset, use
protocol fixtures, and retain an isolated Python bridge as a temporary contingency.

### Cross-platform BLE differences

Mitigation: keep platform identifiers opaque, post-filter discoveries, use explicit
connection state machines, test native OS runners in CI, and never expose btleplug types
outside drivers.

### Immediate-mode UI CPU cost

Mitigation: keep long histories out of egui widget construction, cache derived display
data, use retained custom wgpu resources for the main visualizations, and profile before
customizing the event loop.

### UI/service version skew

Mitigation: version the protobuf contract, negotiate on connect, retain backward
compatibility for one release where practical, and display upgrade instructions.

### Packaging a persistent user process

Mitigation: implement and test per-user startup separately on each OS; do not require
administrator/root privileges merely to run the app.

### Porting algorithm bugs

Mitigation: establish golden behavior first, then make algorithm changes deliberately
with fixtures, versioned metrics, and documented tolerances.

## 15. Definition of done

The rewrite is complete when a normal user can install it on each target OS, connect the
Polar H10 and Go Direct belt, close and restart the UI without reconnecting either
device, run and record the HRV and pranayama training views with smooth GPU rendering,
recover cleanly from device and UI failures, replay a session deterministically, and
inspect meaningful diagnostics when anything goes wrong.

All supported builds must pass formatting, warning-free Clippy, tests, release builds,
and the relevant native smoke tests. Performance and reliability targets must have
recorded evidence rather than subjective assessment.
