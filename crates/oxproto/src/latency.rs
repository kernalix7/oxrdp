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
    ///
    /// Saturates rather than overflowing: a session pathological enough to hit this (each of a
    /// full window's samples within reach of `u64::MAX`) has bigger problems than a wrong mean,
    /// but silently wrapping into a small, plausible-looking number would hide that.
    pub fn mean(&self) -> Option<u64> {
        if self.is_empty() {
            return None;
        }
        let sum = self.iter().fold(0u64, u64::saturating_add);
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
///
/// This is the standard SNTP simplification of NTP's four-timestamp exchange: the agent reports
/// one clock reading (`agent_us`) rather than separate receive/send times, so the offset assumes
/// the agent's own processing between the two is negligible, and RTT/2 assumes the network
/// delays both directions equally. Neither is guaranteed by a TCP connection through a VM port
/// forward — the true error can exceed half the RTT if the path is asymmetric. What *is*
/// guaranteed: [`ClockSync::offset_error_bound_us`] never overstates precision, because it is
/// always half the RTT of the exact sample the current offset came from, not a smoothed or
/// assumed figure. See `OXPROTO.md` §12.1.
#[derive(Debug, Clone)]
pub struct ClockSync {
    pending: std::collections::HashMap<u32, u64>,
    rtt: Samples,
    offset: Option<i64>,
    /// RTT of the sample `offset` was computed from — kept paired with the estimate it
    /// describes rather than left for the caller to separately match against `rtt()`.
    offset_rtt: Option<u64>,
    /// Send time (client clock) of the most recent sample actually applied to `offset`, so a
    /// Pong that arrives out of order relative to when its Ping was sent cannot overwrite a
    /// newer estimate with a stale one. Not reachable today — a single TCP connection delivers
    /// in order, and the agent answers Pings strictly in the order it receives them — but the
    /// estimator should not depend on that holding forever, especially with QUIC planned
    /// (`OXPROTO.md` §2).
    applied_sent_us: Option<u64>,
}

impl ClockSync {
    /// A tracker keeping `window` RTT samples.
    pub fn new(window: usize) -> Self {
        Self {
            pending: std::collections::HashMap::new(),
            rtt: Samples::new(window),
            offset: None,
            offset_rtt: None,
            applied_sent_us: None,
        }
    }

    /// Record that a Ping with `seq` was sent at client time `sent_us`.
    pub fn on_ping_sent(&mut self, seq: u32, sent_us: u64) {
        self.pending.insert(seq, sent_us);
    }

    /// Record a Pong. `agent_us` is the agent's clock from the message, `now_us` the client
    /// time it arrived. Returns the round-trip time, or `None` if this seq was never sent
    /// (a duplicate or forged Pong must not corrupt the estimate).
    ///
    /// The RTT sample is always recorded, even for a Pong answering an older Ping than the one
    /// the current offset estimate is based on. The offset itself is not updated by such a
    /// sample — `applied_sent_us` only moves forward — so a reordered reply can add a data
    /// point to `rtt()`'s statistics without ever making the offset estimate worse.
    pub fn on_pong(&mut self, seq: u32, agent_us: u64, now_us: u64) -> Option<u64> {
        let sent_us = self.pending.remove(&seq)?;
        let rtt = now_us.saturating_sub(sent_us);
        self.rtt.push(rtt);

        if self
            .applied_sent_us
            .is_none_or(|applied| sent_us >= applied)
        {
            let rtt_half = rtt / 2;
            let midpoint = sent_us.saturating_add(rtt_half);
            // Widened to i128 so the subtraction can never overflow regardless of how the two
            // u64 clocks relate, then clamped into the i64 range every caller of `offset_us`
            // expects. The previous version computed this as an unsigned `saturating_sub` before
            // casting to `i64`, which floors at zero instead of going negative whenever the
            // agent's clock reads behind the client's — silently reporting "no skew" for exactly
            // half of all possible clock-skew directions. There was no test for that direction.
            let offset = (i128::from(agent_us) - i128::from(midpoint))
                .clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64;
            self.offset = Some(offset);
            self.offset_rtt = Some(rtt);
            self.applied_sent_us = Some(sent_us);
        }
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

    /// Half the RTT of the sample [`ClockSync::offset_us`] was computed from: the error bound
    /// the symmetric-path assumption promises, *if* the path really is symmetric. An offset
    /// without this is not defensible on its own; a caller quoting `offset_us()` should always
    /// quote this alongside it.
    pub fn offset_error_bound_us(&self) -> Option<u64> {
        Some(self.offset_rtt? / 2)
    }

    /// Convert an agent timestamp into client time using the current offset estimate.
    pub fn agent_to_client(&self, agent_us: u64) -> Option<u64> {
        Some(agent_to_client(agent_us, self.offset?))
    }
}

/// Converts an agent-clock timestamp into the equivalent client-clock timestamp, given
/// `offset_us = agent_clock - client_clock`. `unsigned_abs` rather than negating `offset_us`
/// directly: negating `i64::MIN` overflows i64's range, and while no session runs anywhere near
/// long enough to produce an offset that large, a conversion this central should not carry a
/// panic that a pathological-but-representable input could reach.
fn agent_to_client(agent_us: u64, offset_us: i64) -> u64 {
    if offset_us >= 0 {
        agent_us.saturating_sub(offset_us.unsigned_abs())
    } else {
        agent_us.saturating_add(offset_us.unsigned_abs())
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
    fn clock_sync_estimates_rtt_and_offset_when_the_agent_is_ahead() {
        let mut c = ClockSync::new(8);
        // Client sends at 1000, agent replies stamped 5100, client receives at 1200.
        // rtt = 200, offset = 5100 - (1000 + 100) = 4000.
        c.on_ping_sent(1, 1000);
        assert_eq!(c.on_pong(1, 5100, 1200), Some(200));
        assert_eq!(c.rtt().last(), Some(200));
        assert_eq!(c.offset_us(), Some(4000));
        assert_eq!(c.offset_error_bound_us(), Some(100), "half the 200us RTT");
        // An agent timestamp of 5100 corresponds to client time 1100.
        assert_eq!(c.agent_to_client(5100), Some(1100));
    }

    /// The case the original implementation got wrong: it computed the offset as an unsigned
    /// `saturating_sub` before casting to `i64`, which floors at zero instead of going negative,
    /// so every agent-behind-client session silently reported "no clock skew" instead of the
    /// true offset. Nothing here was reachable via the crate's own tests before this one.
    #[test]
    fn clock_sync_estimates_a_negative_offset_when_the_agent_is_behind() {
        let mut c = ClockSync::new(8);
        // Client sends at 10_000, agent replies stamped 8_000 (2000us behind), client receives
        // at 10_200. rtt = 200, offset = 8000 - (10000 + 100) = -2100.
        c.on_ping_sent(1, 10_000);
        assert_eq!(c.on_pong(1, 8_000, 10_200), Some(200));
        assert_eq!(c.offset_us(), Some(-2_100));
        // An agent timestamp of 8_000 corresponds to client time 10_100 — later than the agent
        // timestamp's own numeric value, because the agent's clock reads behind.
        assert_eq!(c.agent_to_client(8_000), Some(10_100));
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

    /// A Pong answering an older Ping than the one behind the current estimate must not replace
    /// a good estimate with a worse one — it still counts for RTT statistics, just not for
    /// `offset_us`. Not reachable over the current TCP transport (see the field doc on
    /// `ClockSync::applied_sent_us`), but the estimator must not silently assume that forever.
    #[test]
    fn a_pong_for_an_older_ping_updates_rtt_but_not_the_stale_offset() {
        let mut c = ClockSync::new(8);
        c.on_ping_sent(2, 2_000);
        c.on_ping_sent(1, 1_000);
        // The newer ping (seq 2, sent later) is answered first.
        assert_eq!(c.on_pong(2, 6_000, 2_200), Some(200));
        assert_eq!(c.offset_us(), Some(3_900)); // 6000 - (2000 + 100)

        // Now the older ping's reply arrives, with a wildly different (implausible) offset. If
        // applied, this would corrupt the estimate with stale data.
        assert_eq!(
            c.on_pong(1, 50_000, 1_200),
            Some(200),
            "still a valid RTT sample"
        );
        assert_eq!(
            c.offset_us(),
            Some(3_900),
            "the older ping's reply must not overwrite the newer estimate"
        );
        assert_eq!(
            c.rtt().len(),
            2,
            "both round trips still count as RTT samples"
        );
    }
}
