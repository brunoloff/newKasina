# Developer diagnostics

Run helpers from the repository root. Normal desktop use is described in the
[README](../README.md); these commands support repeatable device and rendering tests.

## Device soak tests

For an eight-hour hardware run with append-only diagnostic snapshots every ten seconds:

```sh
scripts/run-hardware-soak 8h
```

The first three arguments are duration, output path, and diagnostic interval.
Additional arguments are forwarded to the service:

```sh
scripts/run-hardware-soak 8h hardware-soak.jsonl 10 \
  --polar-id POLAR_ID --go-direct-id GO_DIRECT_ID
```

A short run such as `scripts/run-hardware-soak 2m` is useful before an overnight test.
Saved peripheral identifiers are preferences; device discovery retains its
advertised-name/service fallback. Generated hardware-soak logs are private and
ignored by Git. They include device states, reconnect counts, stream sequences,
retention/loss/age, client counts, and transport lag.

## Rendering benchmarks

Run from a graphical desktop with access to the GPU. The Diagnostics panel identifies
the adapter, backend, frame-time percentiles, and upload counters.

```sh
scripts/run-render-benchmark 30 render-benchmark-60hz.json 8000 60 60
scripts/run-render-benchmark 30 render-benchmark-120-display.json 8000 120 60
```

Arguments select measured duration, output path, particle count, actual display
refresh rate, and optional application performance target. The benchmark warms up
for two seconds, records measurements, writes JSON, and exits.

Reports include source revision, OS/architecture, GPU/backend, viewport and scale,
presentation settings, achieved FPS, frame-interval and UI-CPU percentiles, upload
size, and the p99 budget result. Software rendering is ineligible to pass the native
GPU target. Reports also include surface recovery counters and device-loss details.

Root-level benchmark outputs are ignored. Keep selected, contextualized evidence in
`docs/validation` when it is useful to future reviewers.
