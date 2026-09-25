# ThoughtStream USB acquisition

The reference implementation is Bruno's `pyThoughtstream.py` and
`pyThoughtstream3.py` in `/home/bruno/Crapbox/Repositories/Programming/pyThoughtstream`.
In particular, `open_connection()` defines the serial settings and `getvalues()`
defines the packet and conversion. This is a port of that code, not a claim of
independent manufacturer-protocol verification.

## Wire format

The USB device exposes a serial port: **19200 baud, 8 data bits, no parity, one stop
bit, no flow control**. The Python program only reads; it sends no start/stop or
configuration commands. The Rust driver follows that behavior using asynchronous
[tokio-serial](https://docs.rs/tokio-serial/5.5.0/tokio_serial/).

| Offset | Meaning |
| --- | --- |
| 0–2 | `A3 5B 08` header and total packet length |
| 3–4 | Unsigned 16-bit ADC reading, big endian |
| 5 | Status byte |
| 6–7 | Unsigned 16-bit sum of bytes 0–5, big endian |

`resistance_ohms = 7700010000 / adc - 470000`. The Python UI divides this by 1000
to display kilohms; the service stores **ohms**, without UI averaging. For example,
`A3 5B 08 27 10 04 01 41` means ADC 10000, resistance 300001 ohms, new data.

The decoder validates checksums before using readings, handles partial and multiple
packets per read, and searches for the next header after corruption. A zero ADC or
negative resistance retains an invalid ADC sample but emits no resistance value.
The next resistance sample reports a gap. Checksum failures never produce samples.

## Streams and quality

Both `thoughtstream_adc` (`count`) and `skin_resistance` (`ohm`) flow through service
retention, gRPC history/subscriptions, diagnostics, and session recordings. Protocol
1.2 appends stream IDs 7 and 8 and device kind 4; existing IDs are unchanged.
Samples use service receive timestamps because the packet has no device timestamp.

| Device status | Shared sample flags |
| --- | --- |
| Bit 0: probe error | `PROBE_ERROR` (bit 6) and `SOURCE_INVALID` (bit 1) |
| Bit 1: low battery | `LOW_BATTERY` (bit 3) |
| Bit 2 clear: no new data | `STALE` (bit 5) |
| Bit 3: recalculation occurred | `RECALIBRATED` (bit 4) |

Every checksum-valid packet is retained, including repeated/stale values, with these
flags. This preserves the Python reader's acquisition behavior without treating
stale or probe-error values as clean readings. The unassigned upper status bits have
no defined meaning in the reference. `AFTER_GAP` (bit 2) marks resynchronization or
the first reading after reconnect. Flags appear beside the latest Raw Signals values;
device status also reports probe/battery/freshness state. The native ADC remains
available in Raw Signals. The dedicated ThoughtStream panel provides averaged
resistance, recent history and optional sound feedback; see [panel controls](../thoughtstream-panel.md).

The simulated service and the app's local simulation include synthetic resistance
and consistent ADC counts at 1 Hz. This is a demo rate, not a hardware rate claim.

## Run and select the port

Open **newKasina Sensor Service** from the desktop application launcher. Hardware
mode automatically checks USB devices advertising ThoughtStream and CP2102 adapters
(the USB bridge in the tested device). Each candidate must produce a checksum-valid
ThoughtStream packet before the driver accepts it. Discovery sends no device commands;
unresponsive candidates are skipped after two seconds. Other serial adapters can be
selected manually.

If discovery does not find the device, open the service's tray menu and choose
**Choose ThoughtStream port…**. The submenu provides **Find automatically**, a
**Refresh port list** action, and a button for each available serial port with its
USB product name and port name. It also refreshes automatically as devices are
plugged in. The active selection has a checkmark. A saved port that is currently
absent stays visible and can be replaced or reset to automatic discovery.

Selecting a port reconnects only ThoughtStream immediately; Polar/Go Direct,
in-memory sequences, RPC clients, and active session recording keep running.
Selections are saved atomically in `service-devices.json` in the service's per-user
configuration directory and restored on the next launch. Linux selections use stable
`/dev/serial/by-id/...` paths when available. Selecting **Find automatically** clears
the saved explicit port. A save error is reported in the tray while the selection
still works for the current run.

The terminal options remain available for testing and headless use:

```sh
scripts/cargo-local run --release -p kasina-service -- --source thoughtstream
scripts/cargo-local run --release -p kasina-service -- --source hardware
```

`--thoughtstream-port PORT` optionally overrides the saved preference at startup;
normal desktop use does not require it. The port chooser can replace that override
for the current run and save a future default. If a launcher explicitly includes an
override, that override will take priority again when the process next starts.

A port is marked connected only after a valid packet arrives. Ten seconds without
valid data after identification triggers reconnect with bounded backoff. Shutdown
and manual port changes cancel reads, backoff, and event-channel waits. Closing the
client app does not close the device. The operating-system permission to open the
USB serial device is still required (see the permanent permission setup below).

## Verification

Unit tests cover the reference conversion, arbitrary packet splits, coalesced reads,
checksum failures, overlapping headers, status flags, invalid ADCs, timeout, and
cancellation under backpressure. Hardware validation must additionally check the
actual USB identity, packet rate, resistance display against the original program,
probe/battery behavior, and unplug/replug recovery. Run the original Python reader
and this service one at a time so they do not compete for the port.

### Local hardware check (2026-09-10)

The connected adapter enumerated as Silicon Labs CP2102 (`10c4:ea60`, serial
`0001`) at `/dev/ttyUSB0`, with stable path
`/dev/serial/by-id/usb-Silicon_Labs_CP2102_USB_to_UART_Bridge_Controller_0001-if00-port0`.
These generic adapter identifiers identify a candidate for automatic discovery;
only a valid ThoughtStream packet confirms the device.

A 12-second isolated run of the release measurement service reached the driver and
retried independently, but the host denied access to the serial port. The node was
`root:uucp`, mode `0660`, and the current user was not in `uucp`. The user then
applied the temporary ACL below, and acquisition succeeded.
A temporary per-device grant can be applied from the user's terminal:

```sh
sudo setfacl -m u:bruno:rw /dev/ttyUSB0
```

This grant ends when the device node is recreated after unplugging. Once permission
is available and the device is on, the bounded smoke reader can inspect the actual
values without a GUI or persistent recording:

```sh
scripts/cargo-local run -p kasina-thoughtstream --example read -- /dev/ttyUSB0 10
```

### Successful live validation

After the permission grant, the driver read 199 packets in a 10-second smoke check.
A separate release-service test measured **20.02 Hz**, recorded **102 resistance
samples plus 102 ADC samples**, and checked all 102 conversions against the Python
formula. All recorded samples had quality flags 0; the probe and battery reported OK.
The recording dropped no samples, and the device needed no reconnections.

The test subscribed over authenticated loopback gRPC, closed the client connection
for two seconds, then recovered 80 scalar samples with contiguous sequences and no
gaps. The same recording stayed active throughout, and the service shut down cleanly.
Evidence for this local run is in
`/tmp/newkasina-thoughtstream-verified-l94hpbxq/summary.json` and its adjacent temporary
session directory. Physical unplug/replug and error-flag transitions remain untested.

### Permanent serial-port permission on this Manjaro system

Add the user to the existing serial-device group:

```sh
sudo usermod -aG uucp bruno
```

Then fully log out of the desktop and log in again, or reboot. This gives new desktop
applications and the measurement service the updated group membership. `id -nG`
should include `uucp` in the new session. Access then survives unplugging and rebooting;
the per-device `setfacl` command is no longer needed. The `-a` option preserves existing
supplementary groups. See the [Arch serial-access guidance](https://wiki.archlinux.org/title/Working_with_the_serial_console)
and [usermod manual](https://man.archlinux.org/man/usermod.8).

The CP2102 adapter is now discovered automatically. The tray port chooser provides
a saved fallback, so no command-line port argument is needed.
