# ADR 0001: persistent per-user service and loopback gRPC

Status: accepted, 2026-08-12

## Context

The UI must be restartable without reconnecting the Polar H10 or Go Direct respiration
belt. Linux, Windows, and macOS all support a long-lived process in the user's session.
The data rates are modest, but the boundary must support snapshots, live streaming,
cancellation, and protocol evolution.

## Decision

Run acquisition in a per-user `kasina-service` process. Communicate over a protobuf/tonic
API bound only to IPv4 loopback. Authenticate every RPC with a random token stored in the
user configuration directory. Negotiate protocol major/minor versions on connection.

## Consequences

The UI can restart independently and recover retained samples by sequence. Native local
sockets could reduce the already negligible transport overhead but would require separate
Unix-socket and Windows-pipe transports. Loopback authentication is required because
biometric data and device controls must not be exposed to unrelated local users.

