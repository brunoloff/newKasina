# Foundation validation — 2026-08-12

Revision under test: the commit containing this report.

## Automated gates

All commands used the ignored project-local dependency/toolchain caches:

```sh
export CARGO_HOME="$PWD/.cargo-local"
export RUSTUP_HOME="$PWD/.rustup-local"

cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo build --workspace --release
```

Results:

- formatting: pass;
- Clippy with warnings denied: pass;
- 23 unit/integration tests plus doc tests: pass;
- optimized workspace build: pass.

The test coverage includes bounded retention and cursor gaps, replay ordering, low-pass
and RMSSD reference calculations, Polar packets, Go Direct commands/reassembly/values,
WGSL parsing, protocol authentication/version skew, service broadcast lag, UI queue
backpressure, live/history overlap removal, multi-device supervision, and service
continuity across client restart.

## Release process smoke test

The Codex sandbox had no `/dev/dri`, so the smoke run used Xvfb and Mesa llvmpipe through
wgpu's OpenGL backend. This validates the process and render paths but is deliberately not
a native-GPU performance claim.

```sh
Xvfb :99 -screen 0 1280x900x24 -nolisten tcp

target/release/kasina-service \
  --port 18871 \
  --token-path /tmp/newkasina-smoke-token \
  --lock-path /tmp/newkasina-smoke.lock \
  --source simulated

DISPLAY=:99 WAYLAND_DISPLAY= WGPU_BACKEND=gl WGPU_POWER_PREF=low \
  target/release/kasina-app \
  --endpoint http://127.0.0.1:18871 \
  --token-path /tmp/newkasina-smoke-token
```

Observed:

- authenticated connection and live simulated HR, RR, and respiration values;
- history plotted immediately on startup;
- exact service instance ID survived UI termination/restart;
- the restarted UI recovered 1,183 retained samples, up from 180 before restart;
- 8,000 instanced particles rendered through the custom wgpu callback;
- resize from 1180x780 to 900x620 and unmap/map completed without a wgpu validation or
  surface error;
- final diagnostics reported zero explicit gaps, zero inferred sequence gaps, zero
  duplicates, zero UI batch drops, and zero service transport lag;
- callback preparation averaged roughly 0.01 ms and uploaded 32 bytes per animated frame
  under llvmpipe; overall software frame timings are not comparable to the target GPU.

## Native checks still required

From the normal desktop session, run the service and app in release mode, open the GPU
visualizer, and record Diagnostics at 60 Hz and 120 Hz where supported. Exercise F11,
window resizing, minimizing, and a mixed-DPI monitor move. Record adapter, backend,
resolution, refresh rate, instance count, average/p95/p99 frame time, and upload bytes.

Physical Milestone 3 checks additionally require the Polar H10 and respiration belt:
side-by-side values against pyKasina, power-cycle recovery, and the eight-hour soak.
