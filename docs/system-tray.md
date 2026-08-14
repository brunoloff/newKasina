# Measurement-service system tray

`kasina-service` runs as a small desktop system-tray application by default. The native
tray event loop and the Tokio acquisition runtime are separate, so opening menus never
blocks Bluetooth, recording, or RPC traffic. Stopping the measurement runtime leaves the
tray available to start it again; quitting the tray stops acquisition and finalizes an
active recording before the process exits.

The implementation uses the maintained `tray-icon` and Tao crates. It supports Linux,
Windows, and macOS. iOS and Android do not expose the desktop system-tray model, and are
outside the current desktop-only target.

## Icon

The generated 64-pixel icon has two rows:

- a heart and its Polar H10 connection light;
- breath waves and their Go Direct connection light.

The light colors mean:

- green: connected;
- amber: discovering, connecting, or reconnecting;
- red: disconnected or an acquisition error;
- gray: the server is stopped or that device family is not configured.

The image is generated directly from status, with no external icon file to lose during
packaging. Windows and macOS also receive a concise hover tooltip. Linux AppIndicator
hosts expose the full detail through the menu; depending on the panel, either left or
right click may open that same menu.

## Menu

The menu reports:

- service lifecycle, uptime, and connected client count;
- Polar and Go Direct connection state, detail, newest-sample age, and retry count;
- recording state, label, sample count, and dropped-sample count;
- the result of the latest tray action.

Actions include starting/stopping a recording, opening the desktop client, opening the
recordings folder, stopping/restarting acquisition, and quitting both tray and server.
A recording started from the tray uses the label `Tray recording`; use the desktop app's
Settings tab when a custom label or notes are wanted.

## Run modes

Build and run the hardware service with its tray:

```sh
scripts/cargo-local build --workspace --release
./target/release/kasina-service --source hardware
```

The existing simulated source remains the default, so this is enough for a no-device
functional test:

```sh
./target/release/kasina-service
```

Use headless mode when no desktop session is present:

```sh
./target/release/kasina-service --headless --source hardware
```

The hardware-soak helper automatically selects headless mode.

## Command link

The standard location for locally built administrator-installed commands is
`/usr/local/bin`:

```sh
sudo ln -sfn /home/bruno/Crapbox/Repositories/newKasina/target/release/kasina-service \
  /usr/local/bin/kasina-service
```

If a link under `/usr/bin` is explicitly preferred:

```sh
sudo ln -sfn /home/bruno/Crapbox/Repositories/newKasina/target/release/kasina-service \
  /usr/bin/kasina-service
```

The link continues to point at the current release binary after later Cargo rebuilds.
Creating either system-wide link requires root only for the link itself; building and
running newKasina does not require root.

On Debian/Ubuntu systems that do not already have the Linux tray headers, install GTK 3
and either AppIndicator implementation before building:

```sh
sudo apt install libgtk-3-dev libayatana-appindicator3-dev
```

This development machine already has the required packages.
