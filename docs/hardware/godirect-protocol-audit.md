# Go Direct BLE protocol audit

Date: 2026-08-12

## Decision

Implement the respiration-belt subset natively in Rust with `btleplug`. Keep a Python
bridge only as an explicit troubleshooting fallback; the audited protocol is compact
enough that a permanent Python runtime dependency is not justified.

This is a feasibility decision, not a completed hardware validation. The native codec
and transport compile and have specification-derived tests, but must still be compared
against a physical belt before Milestone 3 can be marked complete.

## Sources audited

- Vernier's BSD-3-Clause [`godirect-js`](https://github.com/VernierST/godirect-js),
  commit `800400ad0beddc567ce3686db70db13e9afddad5` (2025-03-17).
- Vernier's BSD-3-Clause [`godirect-py`](https://github.com/VernierST/godirect-py),
  commit `08752828a243984442c3ae6a545fd4d6e4f0b2b3` (2025-09-04).
- The existing read-only `pyKasina/measurement_server.py`, which selects channel 1 and
  requests a 100 ms period (10 samples/second).

The JavaScript and Python implementations agree on the framing, BLE characteristics,
20-byte write chunking, rolling counter, and streamed measurement layouts.

## BLE transport

| Purpose | UUID | Operation |
| --- | --- | --- |
| Service | `d91714ef-28b9-4f91-ba16-f0d9a604f112` | Discover |
| Command | `f4bf14a6-c7d5-4b6d-8aa8-df1a7c83adcb` | Write without response, 20-byte chunks |
| Response | `b41e6675-a329-40e0-aa01-44d2f444babe` | Notify |

Discovery must post-filter advertisements by saved platform ID, GDX name, or service
UUID rather than relying exclusively on a BlueZ scan filter.

## Command framing and required sequence

Commands have a four-byte envelope followed by a subcommand:

```text
58 length descending_counter checksum subcommand...
```

The checksum is the wrapping sum of every packet byte except the checksum byte itself.
On a fresh connection the required subset is:

1. subscribe to the response characteristic;
2. send initialization command `0x1a` and its 20-byte initialization payload;
3. send `0x1b` with the measurement period as little-endian microseconds;
4. send `0x18` with a little-endian channel bit mask;
5. consume measurement notifications beginning with `0x20`;
6. send `0x19` before a clean disconnect.

Command responses echo the subcommand at byte 4 and rolling counter at byte 5. Only one
command may be outstanding at a time. Notifications can be fragmented, so the declared
length at byte 1 drives reassembly.

## Measurement layouts implemented

| Type | Layout |
| --- | --- |
| `0x06` normal real32 | 16-bit channel mask, value count, then interleaved LE `f32` values |
| `0x07` wide real32 | 32-bit channel mask, value count, then interleaved LE `f32` values |
| `0x08`, `0x0a` single/aperiodic real32 | channel number, value count, LE `f32` values |
| `0x09`, `0x0b` single/aperiodic int32 | channel number, value count, LE `i32` values |
| `0x0c`–`0x0e` | start time, dropped-count, and period metadata; recognized, not values |

The Rust driver currently exposes channel 1 as raw `RespirationForce` values with the
domain unit `device`, matching pyKasina's behavior. Sensor-info parsing and a physical
capture must confirm the actual device description and unit before a user-facing unit is
claimed.

## Remaining physical validation

- Record the belt's advertised name, platform ID, firmware, channel-1 description, and
  unit.
- Capture consent-safe request/response/measurement bytes and add them as fixtures.
- Compare native and Python values sample-for-sample for at least 30 minutes at 100 ms.
- Power-cycle the belt, disable/enable the adapter, and verify automatic reconnection.
- Run a dual-device eight-hour soak and retain sequence-gap and reconnect diagnostics.
