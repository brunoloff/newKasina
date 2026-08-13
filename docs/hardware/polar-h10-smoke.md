# Polar H10 physical smoke

Date: 2026-08-13

## Result

The native `btleplug` driver discovered a worn Polar H10 through BlueZ while the service
was supervising a Go Direct respiration belt concurrently. The H10 connected without a
retry, discovered its services, subscribed to the standard Heart Rate Measurement
characteristic, and produced both heart-rate and RR-interval samples.

At 44 seconds of service uptime, diagnostics retained 24 heart-rate samples, 24 RR-
interval samples, and 239 respiration-force samples. Both devices reported connected,
the newest Polar sample was 179 ms old, the newest respiration sample was 26 ms old, and
transport lag was zero. The platform peripheral identifier is deliberately omitted from
this tracked report.

The Go Direct setup initially timed out on one command during this dual-device start. Its
supervisor retried once, completed metadata and streaming setup, and then remained
connected. This is useful evidence that the two independent supervisors recover without
preventing the other device from starting.

## Remaining physical validation

- Compare decoded heart rate and every RR interval against a reference application or
  consent-safe capture.
- Power-cycle the H10 and verify automatic reconnection and sequence continuity.
- Restart the graphical client repeatedly while both device connections remain active.
- Disable and re-enable the Bluetooth adapter, then test suspend and resume.
- Complete the eight-hour dual-device soak and retain its private JSONL report.
