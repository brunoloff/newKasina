# Measurement service

The first sidebar tab shows the actual measurement service even when signal panels
are in display-only simulation mode. Heart, breath, and thought cards report live
readings, missing devices, reconnect attempts, and stopped sensors. Stale data and
disconnected services never appear as a live connection.

## Launch and ownership

| Platform / mode | No service running | Existing service | Close the app |
| --- | --- | --- | --- |
| Windows/macOS, default | Start hardware acquisition in this app | Connect to it | Stop only the service this app owns |
| Linux, default | Start button opens the sibling tray service | Connect to it | Leave the tray service running |
| `--service-mode embedded` | Start in this app | Connect to it | Stop only the owned service |
| `--service-mode external` | Wait for a service | Connect to it | Leave the service running |

The embedded and standalone hosts use the same library, token, port, singleton
lock, device preferences, recordings, and drivers. Binding and singleton ownership
happen before hardware is opened. A busy port or unreadable token is an error,
never an invitation to open a second copy of the sensors. An app using a custom
endpoint or token path only connects; it does not start hardware automatically.

On Linux, keep `kasina-service` beside `kasina-app` for the Start button. The service
is independently restartable from the tray as before. If the tray is open with its
server stopped, the app's Start button asks that same tray to resume measurements;
it does not open another tray or change the tray's chosen settings.

## Controls

- Enable or disable individual sensors without interrupting the others.
- Reconnect a sensor by finishing its current connection before starting a new one.
- Select ThoughtStream's USB port or return to automatic discovery. Preferences
  are shared with the tray and survive restarting the service.
- Start/stop recordings and open the completed recording's folder.
- Stop measurements when this app owns the service. Independent servers have no
  app-owned Stop control, preventing an accidental shutdown of another workflow.

Older service versions still supply readings, but their unsupported sensor controls
are unavailable. Restart the tray with the updated service binary to enable them.
Controls are authenticated and are disabled while disconnected; queued commands
from a lost connection are not replayed later.

Closing the integrated app cancels its drivers, closes subscriptions, flushes and
finalizes recordings, then releases the singleton lock. Reopening starts a new
session; client histories and sequence cursors reset when the service instance
changes, so new samples are not mistaken for old duplicates.

## Isolated smoke test

For development and packaged-app validation:

```sh
kasina-app --smoke-test-seconds 3 --smoke-test-output /absolute/path/result.json
```

This opens the real GUI and runs a simulated embedded service with an ephemeral
port and temporary settings, token, lock, and recordings directory. It never opens
physical hardware or attaches to the normal service. Success requires rendered
frames and samples from the owned service's instance. Failure or timeout produces
a failed report/exit status. The app closes and shuts down its service afterward.
