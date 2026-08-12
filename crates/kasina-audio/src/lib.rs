//! Audio feedback command model.
//!
//! The real-time mixer is intentionally deferred until the acquisition/rendering slice
//! is complete. Keeping commands in a separate crate prevents UI coupling.

use std::time::Duration;

/// A non-blocking audio command destined for the future mixer thread.
#[derive(Debug, Clone, PartialEq)]
pub enum AudioCommand {
    /// Play a precomputed heartbeat tone.
    Heartbeat {
        /// Tone frequency.
        frequency_hz: f32,
        /// Tone duration.
        duration: Duration,
        /// Linear amplitude from zero to one.
        volume: f32,
    },
    /// Stop all currently playing feedback.
    StopAll,
}
