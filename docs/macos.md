# newKasina on macOS

The Mac download is a normal application with the measurement service inside it.
No terminal commands, Rust, Python, or separate service are needed on the Mac
that runs it. Linux still supports the separate persistent service and GUI.

## Install and open

1. In **Apple menu → About This Mac**, check the chip: download **AppleSilicon**
   for an Apple M-series chip or **Intel** for an Intel processor. The builds
   target macOS 13 Ventura or newer and require a Metal-capable Mac.
2. Download the corresponding macOS artifact from a successful [GitHub Actions
   run](https://github.com/brunoloff/newKasina/actions). GitHub wraps the download
   in a ZIP; open that ZIP to find the `.dmg` disk image. GitHub sign-in is required
   to download Actions artifacts.
3. Double-click the `.dmg`, then drag **newKasina** to **Applications**.
4. Open **Applications → newKasina**. If macOS blocks this development build,
   follow the first-open instructions below.
5. Allow Bluetooth access when asked. Turn on your sensors and look at the
   **Measurement service** panel for connection status and controls.

The disk image also contains a short, nontechnical **Read me first** guide.
After copying the app to Applications, you can eject the disk image. Closing
the app stops its built-in measurement service; it does not stop a separate
service that was already running.

### First-open permission

These development builds have an **ad-hoc signature** to seal their contents.
They are **not Developer ID signed or notarized by Apple**. Ad-hoc signing is not
an identity check and does not remove Gatekeeper's first-open warning.

If you trust this copy, try opening it once, then go to **System Settings →
Privacy & Security**, scroll to the message about newKasina, select **Open Anyway**,
and confirm. Managed Macs may restrict this option. Apple's instructions are
available in [Open apps safely on your Mac](https://support.apple.com/102445).
There is no need to disable Gatekeeper or run a shell command.

For distribution without this development-build exception, the maintainer must
sign with an Apple Developer ID certificate, enable the hardened runtime, submit
the signed app with `notarytool`, and staple the accepted ticket before creating
the final disk image. That requires an Apple developer account and credentials;
the public CI workflow does not contain or request them. See Apple's
[notarization overview](https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution).

## Connecting sensors

- **Polar H10 and Go Direct respiration belt:** enable Bluetooth, allow newKasina
  under **System Settings → Privacy & Security → Bluetooth**, and close any other
  app currently using the sensor. If permission was previously denied, enable it
  there and reopen newKasina.
- **ThoughtStream USB:** connect its cable and turn it on. Automatic port
  detection runs in the app. If needed, choose its serial port in the Measurement
  service panel. Some older USB-to-serial adapters need a compatible driver from
  their manufacturer; the app does not silently install system drivers.
- **Without hardware:** use simulated signals in Settings to explore the displays.

The app bundle includes `NSBluetoothAlwaysUsageDescription`, which CoreBluetooth
requires to present the Bluetooth permission request. The built-in service runs
in this same application process, so it uses the app's permission. See the
[Apple key documentation](https://developer.apple.com/documentation/bundleresources/information-property-list/nsbluetoothalwaysusagedescription)
and [btleplug's macOS requirements](https://github.com/deviceplug/btleplug#macos).

## What the build checks

The CI workflow uses native **macos-15** (Apple Silicon) and **macos-15-intel**
runners, matching GitHub's [documented runner architectures](https://docs.github.com/en/actions/reference/runners/github-hosted-runners).
Both compile with `MACOSX_DEPLOYMENT_TARGET=13.0`. This is a deployment target;
it does not mean every older OS version has been tested.

For each architecture, CI runs the Rust checks and tests, creates the `.app`,
generates its multi-resolution icon, checks the plist, executable architecture,
deployment target and dynamic-library paths, verifies the ad-hoc signature, and
creates and verifies the DMG. Non-system libraries from the build machine cause
packaging to fail rather than producing a download that depends on Homebrew.

The packaged application is launched through macOS LaunchServices with an
isolated simulated service. The smoke check requires rendered UI frames and
samples received from that exact service instance, writes a JSON report, and
exits. It never opens physical sensors or uses the user's saved settings.

This does not test Bluetooth hardware, USB drivers, a first-run permission prompt,
Gatekeeper's behavior on a downloaded/quarantined copy, or every display/GPU.
A real Mac hardware check remains necessary before calling the app fully tested.
Hosted machines may also lack Metal; a successful compile alone is not a GUI
smoke-test result.

## Building a Mac download locally

On a Mac with Rust, Xcode command-line tools, and Python 3.11 or newer:

```sh
MACOSX_DEPLOYMENT_TARGET=13.0 cargo build --locked --workspace --release
python3 scripts/package-desktop.py
python3 scripts/package-desktop.py --smoke-test
```

The DMG appears in `dist/`. Packaging uses Apple's `sips`, `iconutil`, `plutil`,
`lipo`, `otool`, `codesign`, and `hdiutil` tools. The checked-in 1024-pixel icon
is rendered from the existing Linux SVG, without an external image dependency
at packaging time. To regenerate that source image on a system with librsvg:

```sh
rsvg-convert -w 1024 -h 1024 packaging/linux/icons/org.newkasina.NewKasina.svg \
  -o packaging/macos/newKasina.png
```

`python3 scripts/package-desktop.py --check` validates the shared packaging
resources on any platform. The same packaging script produces a Windows ZIP
containing one double-clickable `newKasina.exe`, and a Linux tarball retaining
both `kasina-app` and `kasina-service`.
