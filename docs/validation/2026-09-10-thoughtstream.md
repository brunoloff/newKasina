# ThoughtStream integration validation — 2026-09-10

- Ported USB serial framing and resistance conversion from Bruno's local
  `Programming/pyThoughtstream` Python sources.
- Added `kasina-thoughtstream`, protocol 1.2 stream/device enums, service source
  selection, recording serialization, tray diagnostics, Raw Signals plots, and
  matching service/client simulation streams.
- `scripts/cargo-local test --workspace --all-features`: passed (75 tests at that
  stage, including six new decoder/session tests).
- Additional `scripts/cargo-local test -p kasina-service --test thoughtstream_serial`:
  passed. Uses an actual pseudo-terminal and the production serial driver to check
  sample subscriptions, cursor recovery after client restart, source/unit/quality
  preservation, and recorded sample/device metadata. The fixture closes its initial
  slave handle before acquisition so the production driver can own the port exclusively.
- `scripts/cargo-local clippy --workspace --all-targets --all-features -- -D warnings`:
  passed.
- `scripts/cargo-local fmt --all --check`: passed.
- `scripts/cargo-local build --workspace --release`: passed; both desktop binaries
  rebuilt. The bounded `kasina-thoughtstream` hardware-reader example also builds.
- Initial live service check reported host `Permission denied`; after the user
  granted serial-port access, the release driver read 199 packets in ten seconds.
- Live authenticated RPC and recording check: passed. Recorded 102 skin-resistance
  and 102 ADC samples at approximately 20.02 Hz; verified 102 conversion pairs,
  canonical units, and quality flags. No recording drops or device reconnections.
- Closing the live RPC client for two seconds preserved acquisition and the active
  recording. Reconnection recovered 80 scalar samples with contiguous sequences and
  no reported gaps. The test service shut down cleanly with exit status 0.
- Local live evidence: `/tmp/newkasina-thoughtstream-verified-l94hpbxq/summary.json`
  and the adjacent temporary session files.
- [Protocol/setup notes](../hardware/thoughtstream-protocol.md) document the observed
  adapter identity, explicit serial-port selection, and permanent `uucp` membership.

The dedicated ThoughtStream feedback interface remains deferred. Native GUI visual
inspection and physical probe/battery/unplug tests were not performed.

## Graphical discovery and port selection follow-up

- Added automatic passive probing of named ThoughtStream devices and CP2102 USB
  adapters. A checksum-valid ThoughtStream packet is required before connection
  is accepted; candidates that send no valid packet are skipped after two seconds.
- Added the native **Choose ThoughtStream port…** tray submenu, USB-first port list,
  refresh action, saved checkmarked selection, and **Find automatically** reset.
- Port changes use a live control channel to restart only ThoughtStream acquisition.
  Settings are saved atomically as `service-devices.json` in the user config directory.
- Driver/service tests pass, including a new pseudo-terminal test switching from a
  missing port to a live port and then to another active port, and a preference
  save/reload/reset test. Workspace Clippy and formatting checks pass.
- Actual graphical service test on the user's desktop: started without a port
  argument and automatically acquired ThoughtStream at approximately 20 Hz.
- Exercised the native D-Bus tray menu actions (the same menu exported to KDE):
  selected CP2102, reset to automatic, and selected CP2102 again. The service
  instance and active recording session were unchanged; 94 scalar samples were
  recorded with zero drops, and reconnection boundaries carried AFTER_GAP.
- Restarted the graphical service with the same isolated configuration, without
  a port argument: saved selection and checkmark were restored, acquisition resumed,
  and the native Quit action exited cleanly. USB port and Other serial ports groups
  were verified in the actual exported native menu.
- Test evidence and isolated settings: `/tmp/newkasina-port-menu-96uue2su/`.

The test used actual native menu activation and live USB acquisition. No physical
unplug/replug or probe/battery error transition was required for this follow-up.

## Three-symbol tray icon

The tray icon now has a heart (pink), breath waves (cyan), and a thought bubble
(violet), each independently grayscale until its device is connected. The separate
status lights were removed. ThoughtStream state participates in icon refreshes.

The updated existing icon test checks each device's isolated color activation and
verifies that all-off, connecting, and error states remain grayscale. Generated
64-pixel RGBA output was visually inspected at enlarged size and at 16, 24, and
32 pixels; see [the preview](tray-icons-2026-09-10.png). The packaged service SVG
uses the same arrangement and palette.
