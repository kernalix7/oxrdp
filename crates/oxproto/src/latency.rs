#![forbid(unsafe_code)]

/// Rolling statistics over the last `N` samples.
#[derive(Debug, Clone)]
pub struct Samples {
    buf: Vec<u64>,
    head: usize,
    len: usize,
    capacity: usize,
}

impl Samples {
    /// A window holding at most `capacity` samples (capacity is clamped to at least 1).
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            buf: Vec::with_capacity(capacity),
            head: 0,
            len: 0,
            capacity,
        }
    }

    /// Record a sample, evicting the oldest when full.
    pub fn push(&mut self, value_us: u64) {
        if self.len < self.capacity {
            self.buf.push(value_us);
            self.len += 1;
        } else {
            self.buf[self.head] = value_us;
            self.head = (self.head + 1) % self.capacity;
        }
    }

    /// Number of samples currently held.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether no samples have been recorded.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Most recent sample.
    pub fn last(&self) -> Option<u64> {
        if self.is_empty() {
            return None;
        }
        let idx = if self.len < self.capacity {
            self.len - 1
        } else {
            (self.head + self.capacity - 1) % self.capacity
        };
        Some(self.buf[idx])
    }

    /// Arithmetic mean, or `None` when empty.
    pub fn mean(&self) -> Option<u64> {
        if self.is_empty() {
            return None;
        }
        let sum: u64 = self.iter().sum();
        Some(sum / self.len as u64)
    }

    /// Smallest sample.
    pub fn min(&self) -> Option<u64> {
        self.iter().min()
    }

    /// Largest sample.
    pub fn max(&self) -> Option<u64> {
        self.iter().max()
    }

    /// The value at the given percentile (0..=100), nearest-rank. `None` when empty.
    pub fn percentile(&self, p: u8) -> Option<u64> {
        if self.is_empty() {
            return None;
        }
        let p = p.min(100);
        let mut sorted: Vec<u64> = self.iter().collect();
        sorted.sort_unstable();
        let rank = (p as usize * sorted.len())
            .div_ceil(100)
            .clamp(1, sorted.len());
        Some(sorted[rank - 1])
    }

    fn iter(&self) -> impl Iterator<Item = u64> + '_ {
        (0..self.len).map(move |i| {
            let idx = if self.len < self.capacity {
                i
            } else {
                (self.head + i) % self.capacity
            };
            self.buf[idx]
        })
    }
}

/// Round-trip time and clock-offset estimation from Ping/Pong exchanges.
#[derive(Debug, Clone)]
pub struct ClockSync {
    pending: std::collections::HashMap<u32, u64>,
    rtt: Samples,
    offset: Option<i64>,
}

impl ClockSync {
    /// A tracker keeping `window` RTT samples.
    pub fn new(window: usize) -> Self {
        Self {
            pending: std::collections::HashMap::new(),
            rtt: Samples::new(window),
            offset: None,
        }
    }

    /// Record that a Ping with `seq` was sent at client time `sent_us`.
    pub fn on_ping_sent(&mut self, seq: u32, sent_us: u64) {
        self.pending.insert(seq, sent_us);
    }

    /// Record a Pong. `agent_us` is the agent's clock from the message, `now_us` the client
    /// time it arrived. Returns the round-trip time, or `None` if this seq was never sent
    /// (a duplicate or forged Pong must not corrupt the estimate).
    pub fn on_pong(&mut self, seq: u32, agent_us: u64, now_us: u64) -> Option<u64> {
        let sent_us = self.pending.remove(&seq)?;
        let rtt = now_us.saturating_sub(sent_us);
        let rtt_half = rtt / 2;
        let offset = agent_us.saturating_sub(sent_us.saturating_add(rtt_half)) as i64;
        self.rtt.push(rtt);
        self.offset = Some(offset);
        Some(rtt)
    }

    /// RTT statistics.
    pub fn rtt(&self) -> &Samples {
        &self.rtt
    }

    /// Current estimate of `agent_clock - client_clock`, once at least one Pong has landed.
    /// Signed, because the agent's clock may be behind the client's.
    pub fn offset_us(&self) -> Option<i64> {
        self.offset
    }

    /// Convert an agent timestamp into client time using the current offset estimate.
    pub fn agent_to_client(&self, agent_us: u64) -> Option<u64> {
        let offset = self.offset?;
        if offset >= 0 {
            Some(agent_us.saturating_sub(offset as u64))
        } else {
            Some(agent_us.saturating_sub((-offset) as u64))
        }
    }
}

/// One frame's measured pipeline, all in microseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameTiming {
    /// Frame this describes.
    pub frame_id: u64,
    /// Agent-side capture to encode-complete.
    pub encode_us: u64,
    /// Encode-complete to the client having it decoded (includes the network).
    pub transit_us: u64,
    /// Decode-complete to on screen.
    pub present_us: u64,
    /// Capture to on screen — the number that actually matters.
    pub total_us: u64,
}

/// End-to-end latency accounting for one window's frames.
#[derive(Debug, Clone)]
pub struct FrameLatency {
    in_flight: std::collections::BTreeMap<u64, FrameRecord>,
    total: Samples,
}

#[derive(Debug, Clone)]
struct FrameRecord {
    captured_us: u64,
    encoded_us: u64,
    decoded_us: u64,
}

impl FrameLatency {
    /// A tracker keeping `window` completed-frame samples.
    pub fn new(window: usize) -> Self {
        Self {
            in_flight: std::collections::BTreeMap::new(),
            total: Samples::new(window),
        }
    }

    /// Record a frame's arrival: its agent-side timestamps, and the client time it was decoded.
    pub fn on_frame(&mut self, frame_id: u64, captured_us: u64, encoded_us: u64, decoded_us: u64) {
        self.in_flight.insert(
            frame_id,
            FrameRecord {
                captured_us,
                encoded_us,
                decoded_us,
            },
        );
        while self.in_flight.len() > 256 {
            let oldest = *self.in_flight.keys().next().unwrap();
            self.in_flight.remove(&oldest);
        }
    }

    /// Record that a frame reached the screen at client time `presented_us`. Returns the
    /// completed timing, or `None` if the frame was never recorded by `on_frame` (it may have
    /// been dropped by the agent's in-flight budget, which is normal).
    pub fn on_presented(
        &mut self,
        frame_id: u64,
        presented_us: u64,
        offset_us: i64,
    ) -> Option<FrameTiming> {
        let rec = self.in_flight.remove(&frame_id)?;

        let captured_client = agent_to_client(rec.captured_us, offset_us);
        let encoded_client = agent_to_client(rec.encoded_us, offset_us);

        let encode_us = rec.encoded_us.saturating_sub(rec.captured_us);
        let transit_us = rec.decoded_us.saturating_sub(encoded_client);
        let present_us = presented_us.saturating_sub(rec.decoded_us);
        let total_us = presented_us.saturating_sub(captured_client);

        let timing = FrameTiming {
            frame_id,
            encode_us,
            transit_us,
            present_us,
            total_us,
        };
        self.total.push(total_us);
        Some(timing)
    }

    /// Statistics over `total_us` for completed frames.
    pub fn total(&self) -> &Samples {
        &self.total
    }

    /// Frames recorded but never presented — the agent's drops plus anything lost.
    pub fn incomplete(&self) -> usize {
        self.in_flight.len()
    }
}

fn agent_to_client(agent_us: u64, offset_us: i64) -> u64 {
    if offset_us >= 0 {
        agent_us.saturating_sub(offset_us as u64)
    } else {
        agent_us.saturating_add((-offset_us) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_window_evicts_oldest() {
        let mut s = Samples::new(3);
        for v in [10, 20, 30, 40] {
            s.push(v);
        }
        assert_eq!(s.len(), 3);
        assert_eq!(s.last(), Some(40));
        assert_eq!(s.min(), Some(20));
        assert_eq!(s.max(), Some(40));
        assert_eq!(s.mean(), Some(30));
    }

    #[test]
    fn empty_samples_report_nothing() {
        let s = Samples::new(4);
        assert!(s.is_empty());
        assert_eq!(s.mean(), None);
        assert_eq!(s.percentile(50), None);
    }

    #[test]
    fn percentiles_use_nearest_rank() {
        let mut s = Samples::new(10);
        for v in [10, 20, 30, 40, 50] {
            s.push(v);
        }
        assert_eq!(s.percentile(0), Some(10));
        assert_eq!(s.percentile(50), Some(30));
        assert_eq!(s.percentile(100), Some(50));
    }

    #[test]
    fn clock_sync_estimates_rtt_and_offset() {
        let mut c = ClockSync::new(8);
        // Client sends at 1000, agent replies stamped 5100, client receives at 1200.
        // rtt = 200, offset = 5100 - (1000 + 100) = 4000.
        c.on_ping_sent(1, 1000);
        assert_eq!(c.on_pong(1, 5100, 1200), Some(200));
        assert_eq!(c.rtt().last(), Some(200));
        assert_eq!(c.offset_us(), Some(4000));
        // An agent timestamp of 5100 corresponds to client time 1100.
        assert_eq!(c.agent_to_client(5100), Some(1100));
    }

    #[test]
    fn an_unsolicited_pong_is_ignored() {
        let mut c = ClockSync::new(4);
        assert_eq!(c.on_pong(99, 1, 2), None);
        assert!(c.rtt().is_empty());
        assert_eq!(c.offset_us(), None);
        // A replayed pong for an already-answered ping is also ignored.
        c.on_ping_sent(1, 100);
        assert!(c.on_pong(1, 500, 200).is_some());
        assert_eq!(c.on_pong(1, 500, 200), None);
    }

    #[test]
    fn frame_latency_splits_the_pipeline() {
        let mut f = FrameLatency::new(16);
        // Agent clock is 4000us ahead of the client's.
        // captured at agent 5000 (client 1000), encoded at agent 5100 (client 1100),
        // decoded on the client at 1300, presented at 1350.
        f.on_frame(7, 5000, 5100, 1300);
        let t = f.on_presented(7, 1350, 4000).expect("frame was recorded");
        assert_eq!(t.frame_id, 7);
        assert_eq!(t.encode_us, 100); // 5100 - 5000
        assert_eq!(t.transit_us, 200); // client 1300 - client 1100
        assert_eq!(t.present_us, 50); // 1350 - 1300
        assert_eq!(t.total_us, 350); // client 1350 - client 1000
        assert_eq!(f.total().last(), Some(350));
        assert_eq!(f.incomplete(), 0);
    }

    #[test]
    fn presenting_an_unknown_frame_is_not_an_error() {
        let mut f = FrameLatency::new(4);
        assert_eq!(f.on_presented(1, 100, 0), None);
    }

    #[test]
    fn negative_intervals_saturate_instead_of_wrapping() {
        let mut f = FrameLatency::new(4);
        // Nonsensical ordering (encoded before captured, presented before decoded).
        f.on_frame(1, 5000, 4000, 1300);
        let t = f.on_presented(1, 1200, 4000).unwrap();
        assert_eq!(t.encode_us, 0);
        assert_eq!(t.present_us, 0);
    }

    #[test]
    fn in_flight_frames_are_bounded() {
        let mut f = FrameLatency::new(4);
        for id in 0..1000 {
            f.on_frame(id, 0, 0, 0);
        }
        assert!(f.incomplete() <= 256, "un-presented frames must be bounded");
    }
}
