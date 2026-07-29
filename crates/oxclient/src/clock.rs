//! The client's monotonic clock.
//!
//! Every client-side timestamp on the wire — `FrameAck.decoded_us`, `FrameAck.presented_us`,
//! `PointerEvent.timestamp` — is microseconds on this one clock (`OXPROTO.md` §12, §13). It is
//! `Copy` because the timestamps now come from several threads: the session task, and one decode
//! worker per window. They have to share an epoch or the agent cannot compare them.

use std::time::Instant;

/// Microseconds since the session started.
#[derive(Debug, Clone, Copy)]
pub struct ClientClock {
    start: Instant,
}

impl ClientClock {
    /// Starts a clock at this instant.
    #[must_use]
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }

    /// The instant this clock counts from.
    ///
    /// Handed to anything that produces timestamps compared against this clock's — the CPU
    /// presenter, above all — so there is one epoch rather than one per component.
    #[must_use]
    pub fn epoch(&self) -> Instant {
        self.start
    }

    /// A clock counting from an epoch someone else already established.
    #[must_use]
    pub fn from_epoch(start: Instant) -> Self {
        Self { start }
    }

    /// Microseconds since [`ClientClock::new`].
    #[must_use]
    pub fn now_us(&self) -> u64 {
        self.start
            .elapsed()
            .as_micros()
            .try_into()
            .unwrap_or(u64::MAX)
    }
}

impl Default for ClientClock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_share_one_epoch() {
        // A decode worker holds a copy; its timestamps must be comparable with the session
        // task's, not offset by however long the worker took to start.
        let clock = ClientClock::new();
        let copy = clock;
        let before = clock.now_us();
        let middle = copy.now_us();
        let after = clock.now_us();

        assert!(before <= middle && middle <= after);
    }
}
