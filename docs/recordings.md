# Session recordings

The persistent measurement service owns recording. A graphical client can disconnect,
restart, or be replaced without reconnecting sensors or ending an active recording.

## Storage layout

The default root is the operating system's private per-user local-data directory. On
Linux this is normally `~/.local/share/newkasina/sessions`. Pass `--recordings-dir PATH`
to `kasina-service` to override it. A session is partitioned by its UTC start date:

```text
sessions/
  2026/08/13/
    20260813T213045Z_evening-breath_4f2a19c0/
      metadata.json
      samples.jsonl
```

The final suffix is the first eight characters of a random session UUID, so repeated
labels and sessions begun in the same second remain distinct. On Unix, service-created
directories use mode `0700` and files use mode `0600`.

## Metadata

`metadata.json` is versioned and replaced atomically. It records:

- session UUID, label, notes, and lifecycle state;
- UTC start/stop timestamps as integer Unix nanoseconds;
- service instance and software version;
- identifiers, names, and families for the service's configured devices;
- total and per-stream sample counts, sequence ranges, timestamp ranges, and recorder
  queue drops;
- the raw-sample filename and a human-readable completion or recovery detail.

The lifecycle state is `recording`, `completed`, `interrupted`, or `error`. A normal stop
flushes and syncs `samples.jsonl` before atomically writing completed metadata.

## Raw samples

Each line of `samples.jsonl` is one independent JSON object:

```json
{"schema_version":1,"stream":"respiration_force","source_id":"godirect:...","sequence":42,"monotonic_time_ns":123456789,"wall_time_unix_ns":1786656645000000000,"device_time_ns":null,"value":17.25,"unit":"device","quality_flags":0}
```

The currently recorded stream names are `respiration_force`, `heart_rate`, `rr_interval`,
and optional `acceleration_x`, `acceleration_y`, and `acceleration_z`. Sequences are
assigned by the service independently for each stream. Use `wall_time_unix_ns` to align
streams between processes or sessions and `monotonic_time_ns` for stable within-service
elapsed timing. RR intervals use integer microseconds (`us`) and heart rate uses `bpm`.
Respiration force currently uses the canonical `device` unit; retaining the selected Go
Direct channel's reported unit in each sample remains planned work.

`quality_flags` is a bit field: bit 0 means simulated input, bit 1 means the source marked
the value invalid or questionable, and bit 2 means a sequence gap preceded the sample.

## Crash recovery

Samples are append-only and each line is flushed as it arrives; file data is additionally
synced at least once per second. If the service disappears before a normal stop,
`metadata.json` may still say `recording`. At its next start the service scans those
sessions, reads all complete JSON lines, rebuilds the stream summaries, ignores one
malformed/truncated tail, and atomically changes the state to `interrupted`. The source
file is never rewritten during recovery.

The recorder uses a bounded queue so acquisition and the UI never block on disk. Queue
saturation is visible as `dropped_samples` in status and metadata rather than silently
pretending the recording is continuous.

## First analysis steps

Inspect a session without special tools:

```sh
jq . metadata.json
jq -c 'select(.stream == "respiration_force")' samples.jsonl | head
jq -s 'group_by(.stream) | map({stream: .[0].stream, samples: length})' samples.jsonl
```

Pandas can read the raw file directly and align each stream to the beginning of the
recording:

```python
import pandas as pd

samples = pd.read_json("samples.jsonl", lines=True)
samples["elapsed_s"] = (
    samples["wall_time_unix_ns"] - samples["wall_time_unix_ns"].min()
) / 1_000_000_000
breath = samples[samples.stream == "respiration_force"].copy()
rr = samples[samples.stream == "rr_interval"].copy()
```

Future analysis code should treat raw data as authoritative and write derived breath
phase, holds, and HRV results separately with an algorithm name and version. CSV export,
recording browsing, explicit gap-event records, and captured app/preset settings remain
planned work.

## ThoughtStream USB

ThoughtStream sessions also contain `skin_resistance` in `ohm` and
`thoughtstream_adc` in `count`, with device kind `thoughtstream` in metadata.
Quality bits 3–6 mean low battery, recalculation, stale data, and probe error;
probe errors also set bit 1 (source invalid). Invalid ADC counts are retained,
but undefined or negative resistance is omitted. Existing field names and schema
version remain unchanged. See [the protocol notes](hardware/thoughtstream-protocol.md)
for packet handling and acquisition semantics.
