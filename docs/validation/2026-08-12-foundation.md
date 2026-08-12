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
- 28 unit/integration tests plus doc tests: pass;
- optimized workspace build: pass.

The test coverage includes bounded retention and cursor gaps, replay ordering, low-pass
and RMSSD reference calculations, Polar packets, Go Direct commands/reassembly/values,
WGSL parsing, protocol authentication/version skew, service broadcast lag, UI queue
backpressure, live/history overlap removal, multi-device supervision, and service
continuity across client restart. Rendering coverage also proves fixed-size normalized
uniform preparation, surface-error recovery routing/counters, and that continuous repaint
is requested only for a visible visualizer, leaving inactive and minimized views
event-driven. A wgpu device-loss callback retains an actionable diagnostic independently
of the service process.

The targeted release benchmark for CPU-side renderer preparation ran with:

```sh
scripts/cargo-local bench -p kasina-render --bench prepare_visual_frame
```

It prepared 20,000,000 varying frames in 60.983 ms (3.049 ns/frame) on this host. The
prepared upload remains 32 bytes for both one and 100,000 instances; particle geometry is
generated in the vertex shader rather than rebuilt on the CPU.

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

The automated benchmark path was also exercised against llvmpipe for one measured second
after a two-second warm-up. It wrote valid JSON, recorded the source tree as dirty, named
the OpenGL CPU adapter and Mesa driver, captured logical/physical viewport and scale,
recorded 89 frames, found the sample count sufficient, and still set `hardware_accelerated`
and `target_met` to `false`. Its frame-interval p99 was 14.469 ms, UI-CPU p99 was 0.355 ms,
and callback p99 was 0.017 ms. All surface-event counters remained zero and device loss
remained null. The harness then atomically installed its JSON output and exited without
intervention. This proves report generation and conservative result classification, not
the 60 Hz performance target.

## Native checks still required

From the normal desktop session, run the automated release benchmark at each externally
confirmed active display refresh rate:

```sh
scripts/run-render-benchmark 30 render-benchmark-60hz.json 8000 60
scripts/run-render-benchmark 30 render-benchmark-120hz.json 8000 120
```

Confirm that `hardware_accelerated` and `target_met` are both true. Also run the service
and app interactively, exercise controls while the visualizer runs, press F11, resize,
minimize/restore, move between mixed-DPI monitors, and confirm that the Diagnostics view
remains free of wgpu validation/surface errors. Device-loss recovery requires a platform-
appropriate forced reset or suspend/resume test; record the procedure with the report.

Physical Milestone 3 checks additionally require the Polar H10 and respiration belt:
side-by-side values against pyKasina, power-cycle recovery, and the eight-hour soak.
