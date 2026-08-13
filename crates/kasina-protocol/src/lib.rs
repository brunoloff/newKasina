//! Versioned wire contract shared by the service and desktop client.

/// Protocol major version. A mismatch is incompatible.
pub const PROTOCOL_MAJOR: u32 = 1;
/// Protocol minor version. New optional fields and methods increment this value.
pub const PROTOCOL_MINOR: u32 = 1;
/// Metadata header used for local authentication.
pub const AUTH_HEADER: &str = "x-kasina-token";

/// Generated protobuf messages and tonic client/server stubs.
#[allow(clippy::all)]
#[allow(missing_debug_implementations)]
#[allow(unused_qualifications)]
pub mod v1 {
    tonic::include_proto!("kasina.v1");
}

/// Construct the standard client handshake.
#[must_use]
pub fn client_hello(name: impl Into<String>, version: impl Into<String>) -> v1::ClientHello {
    v1::ClientHello {
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor: PROTOCOL_MINOR,
        client_name: name.into(),
        client_version: version.into(),
    }
}
