//! Per-window frame pacing and flow control (`docs/design/OXPROTO.md` §12).
//!
//! This is where the project's latency claim is actually kept or lost. Without a budget, the
//! agent encodes as fast as it can and `write_all` hands frames to the kernel socket buffer;
//! on any bandwidth dip that buffer absorbs them and end-to-end latency grows without bound —
//! classic bufferbloat, and precisely the failure that makes a naive streamer feel *worse*
//! than the protocol it replaces.
//!
//! The rule: **never queue a frame, drop it.** When the in-flight budget is full the newest
//! content wins; the stale frame is discarded and the next capture is encoded instead. A late
//! frame has no value — the pixels it describes are already wrong.
//!
//! Pure logic with no IO, so it is unit-tested on the host.

use std::collections::VecDeque;

/// What the capture loop should do with a freshly captured frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pace {
    /// Send it; the returned id must be carried in `FrameData.frame_id`.
    Send(u64),
    /// The budget is full and nothing could be discarded — skip this capture entirely.
    Skip,
}

/// Tracks unacknowledged frames for one window and decides whether to send or skip.
#[derive(Debug)]
pub struct FrameBudget {
    max_in_flight: usize,
    in_flight: VecDeque<u64>,
    next_frame_id: u64,
    dropped: u64,
    sent: u64,
}

impl FrameBudget {
    /// A budget allowing `max_in_flight` unacknowledged frames (clamped to at least 1).
    pub fn new(max_in_flight: u8) -> Self {
        Self {
            max_in_flight: usize::from(max_in_flight).max(1),
            in_flight: VecDeque::new(),
            next_frame_id: 1,
            dropped: 0,
            sent: 0,
        }
    }

    /// Decide what to do with a newly captured frame.
    ///
    /// If the budget is full, the oldest unacknowledged frame is abandoned to make room — the
    /// client will simply never see it, which is correct: by the time it could be delivered it
    /// would already be stale.
    pub fn on_captured(&mut self) -> Pace {
        if self.in_flight.len() >= self.max_in_flight {
            // Abandon the oldest in-flight frame rather than queueing behind it.
            if self.in_flight.pop_front().is_some() {
                self.dropped += 1;
            } else {
                return Pace::Skip;
            }
        }
        let id = self.next_frame_id;
        self.next_frame_id += 1;
        self.in_flight.push_back(id);
        self.sent += 1;
        Pace::Send(id)
    }

    /// Record an acknowledgement. Acks are cumulative: acknowledging a frame also retires
    /// every older one, because a client that presented frame N necessarily discarded the
    /// frames before it.
    ///
    /// An ack for an unknown or already-retired id is ignored rather than treated as an error;
    /// it happens naturally when a frame was dropped by the budget.
    pub fn on_ack(&mut self, frame_id: u64) {
        while let Some(&front) = self.in_flight.front() {
            if front <= frame_id {
                self.in_flight.pop_front();
            } else {
                break;
            }
        }
    }

    /// Frames sent but not yet acknowledged.
    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }

    /// Whether a capture right now would be sent rather than displacing a frame.
    pub fn has_headroom(&self) -> bool {
        self.in_flight.len() < self.max_in_flight
    }

    /// Frames abandoned because the client could not keep up. A rising count means the link or
    /// the client is the bottleneck — worth logging, and worth reacting to by lowering quality.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Frames handed to the transport.
    pub fn sent(&self) -> u64 {
        self.sent
    }

    /// Reset after a stream restart (window resize, codec change), keeping the id sequence
    /// monotonic so late acks for the old stream cannot retire new frames.
    pub fn restart(&mut self) {
        self.in_flight.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sends_while_there_is_headroom() {
        let mut b = FrameBudget::new(2);
        assert_eq!(b.on_captured(), Pace::Send(1));
        assert_eq!(b.on_captured(), Pace::Send(2));
        assert_eq!(b.in_flight(), 2);
        assert!(!b.has_headroom());
    }

    #[test]
    fn drops_the_oldest_instead_of_queueing() {
        let mut b = FrameBudget::new(2);
        b.on_captured();
        b.on_captured();
        // Budget is full: the third capture still sends, displacing frame 1.
        assert_eq!(b.on_captured(), Pace::Send(3));
        assert_eq!(b.in_flight(), 2, "in-flight never exceeds the budget");
        assert_eq!(b.dropped(), 1);
    }

    #[test]
    fn acks_are_cumulative() {
        let mut b = FrameBudget::new(4);
        for _ in 0..4 {
            b.on_captured();
        }
        assert_eq!(b.in_flight(), 4);
        // Acking frame 3 retires 1, 2 and 3.
        b.on_ack(3);
        assert_eq!(b.in_flight(), 1);
        b.on_ack(4);
        assert_eq!(b.in_flight(), 0);
        assert!(b.has_headroom());
    }

    #[test]
    fn a_stale_or_unknown_ack_is_harmless() {
        let mut b = FrameBudget::new(2);
        b.on_captured();
        b.on_ack(999); // far in the future: retires everything known
        assert_eq!(b.in_flight(), 0);
        b.on_ack(1); // already retired
        assert_eq!(b.in_flight(), 0);
    }

    #[test]
    fn frame_ids_stay_monotonic_across_drops_and_restarts() {
        let mut b = FrameBudget::new(1);
        let Pace::Send(first) = b.on_captured() else {
            panic!("expected a send")
        };
        let Pace::Send(second) = b.on_captured() else {
            panic!("expected a send")
        };
        assert!(second > first);
        b.restart();
        let Pace::Send(third) = b.on_captured() else {
            panic!("expected a send")
        };
        assert!(third > second, "ids must not rewind after a restart");
        assert_eq!(b.in_flight(), 1);
    }

    #[test]
    fn a_slow_client_bounds_latency_instead_of_queueing() {
        // The client never acks. Latency is bounded because in-flight is bounded: the agent
        // keeps sending fresh frames and abandoning stale ones rather than growing a queue.
        let mut b = FrameBudget::new(2);
        for _ in 0..1000 {
            assert!(matches!(b.on_captured(), Pace::Send(_)));
            assert!(b.in_flight() <= 2);
        }
        assert_eq!(b.dropped(), 998);
        assert_eq!(b.sent(), 1000);
    }

    #[test]
    fn zero_budget_is_clamped_to_one() {
        let mut b = FrameBudget::new(0);
        assert!(matches!(b.on_captured(), Pace::Send(_)));
        assert_eq!(b.in_flight(), 1);
    }
}
