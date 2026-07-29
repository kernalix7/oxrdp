//! Where a frame's latency actually goes.
//!
//! # What this measures, and what it does not
//!
//! **Capture to present.** From the agent finishing its capture of a window to the client
//! finishing the call that puts those pixels on a surface. That is not glass-to-glass: it
//! excludes the guest's own compositor before capture, and the local display server and monitor
//! after present. Neither is visible from inside this process, so neither is claimed. A real
//! glass-to-glass figure needs a camera pointed at both screens.
//!
//! # The four stages
//!
//! | Stage | From | To | Clock |
//! |---|---|---|---|
//! | capture → encode | `captured_us` | `encoded_us` | agent only |
//! | encode → arrival | `encoded_us` | frame read off the wire | **both** |
//! | arrival → decode | read off the wire | decoder finished | client only |
//! | decode → present | decoder finished | surface written | client only |
//!
//! The "Clock" column is the honesty column. Three of these are differences between two
//! timestamps taken by the *same* clock, so they are exact regardless of how well the two ends
//! agree about time. Only **encode → arrival** spans the two clocks, so only it — and therefore
//! the total, which contains it — depends on the offset estimate and inherits its error.
//!
//! That error is bounded by half the round-trip time and is usually far smaller, since it is
//! really half the *asymmetry* of the path rather than half its length. The report prints the
//! round-trip time next to the numbers for exactly this reason: an end-to-end figure quoted from
//! a 0.3 ms round trip is worth something, and the same figure from a 40 ms round trip is worth
//! much less. The client-only span — arrival to present — is printed separately because it is
//! exact, and it is the part this codebase can actually do something about.
//!
//! # Percentiles, not means
//!
//! A mean hides the stalls, and the stalls are what a remote desktop feels like. p50 says what
//! it is usually like; p99 and the maximum say what makes someone complain.

use std::collections::{BTreeMap, HashMap};

use oxproto::latency::Samples;

/// Completed frames kept per window for the percentile window.
///
/// At 30 fps this is about a minute of history, which is long enough for p99 to mean something
/// and short enough that a report reflects recent behaviour rather than the whole session.
const SAMPLE_WINDOW: usize = 2048;

/// Frames that arrived but never finished, before the oldest is discarded.
///
/// The agent drops frames it has not had acknowledged (`OXPROTO.md` §12), so some frames
/// legitimately never complete and this must not grow without bound.
const MAX_IN_FLIGHT: usize = 256;

/// One frame's journey, in microseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameStages {
    /// Agent-side capture to encode completion. Agent clock only, so exact.
    pub capture_to_encode_us: u64,
    /// Encode completion to the frame being read off the wire. **Spans both clocks.**
    pub encode_to_arrival_us: u64,
    /// Read off the wire to the decoder finishing. Client clock only, so exact.
    pub arrival_to_decode_us: u64,
    /// Decoder finishing to the pixels being written to a surface. Client clock only, so exact.
    pub decode_to_present_us: u64,
    /// Arrival to present: everything this client is responsible for. Exact.
    pub client_us: u64,
    /// Capture to present. Carries the clock-offset error; see the module docs.
    pub total_us: u64,
}

/// A frame seen but not yet finished.
#[derive(Debug, Clone, Copy)]
struct Partial {
    captured_us: u64,
    encoded_us: u64,
    arrived_us: u64,
    decoded_us: Option<u64>,
}

/// One window's accounting.
#[derive(Debug)]
struct WindowLatency {
    in_flight: BTreeMap<u64, Partial>,
    capture_to_encode: Samples,
    encode_to_arrival: Samples,
    arrival_to_decode: Samples,
    decode_to_present: Samples,
    client: Samples,
    total: Samples,
    presented: u64,
    dropped: u64,
}

impl WindowLatency {
    fn new() -> Self {
        Self {
            in_flight: BTreeMap::new(),
            capture_to_encode: Samples::new(SAMPLE_WINDOW),
            encode_to_arrival: Samples::new(SAMPLE_WINDOW),
            arrival_to_decode: Samples::new(SAMPLE_WINDOW),
            decode_to_present: Samples::new(SAMPLE_WINDOW),
            client: Samples::new(SAMPLE_WINDOW),
            total: Samples::new(SAMPLE_WINDOW),
            presented: 0,
            dropped: 0,
        }
    }
}

/// Per-window latency accounting, fed from the three points a frame passes through.
///
/// Disabled unless asked for: [`LatencyMonitor::enabled`] is checked before any work, so a
/// session that is not measuring pays a branch per frame and nothing else.
#[derive(Debug)]
pub struct LatencyMonitor {
    windows: HashMap<u32, WindowLatency>,
    enabled: bool,
}

impl LatencyMonitor {
    /// A monitor that records nothing.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            windows: HashMap::new(),
            enabled: false,
        }
    }

    /// A monitor that records.
    #[must_use]
    pub fn enabled() -> Self {
        Self {
            windows: HashMap::new(),
            enabled: true,
        }
    }

    /// Whether anything is being recorded.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// A frame has been read off the wire, carrying the agent's own two timestamps.
    pub fn on_arrival(
        &mut self,
        window_id: u32,
        frame_id: u64,
        captured_us: u64,
        encoded_us: u64,
        arrived_us: u64,
    ) {
        if !self.enabled {
            return;
        }
        let window = self
            .windows
            .entry(window_id)
            .or_insert_with(WindowLatency::new);
        window.in_flight.insert(
            frame_id,
            Partial {
                captured_us,
                encoded_us,
                arrived_us,
                decoded_us: None,
            },
        );
        // Frames the agent dropped never complete, so the oldest are evicted rather than kept.
        while window.in_flight.len() > MAX_IN_FLIGHT {
            let Some(oldest) = window.in_flight.keys().next().copied() else {
                break;
            };
            window.in_flight.remove(&oldest);
            window.dropped += 1;
        }
    }

    /// A decode worker finished with a frame.
    pub fn on_decoded(&mut self, window_id: u32, frame_id: u64, decoded_us: u64) {
        if !self.enabled {
            return;
        }
        if let Some(window) = self.windows.get_mut(&window_id) {
            if let Some(partial) = window.in_flight.get_mut(&frame_id) {
                partial.decoded_us = Some(decoded_us);
            }
        }
    }

    /// A frame reached a surface. Returns its stages, or `None` if it was never seen arriving —
    /// which happens for frames that were evicted, or when measuring started mid-stream.
    ///
    /// `offset_us` is the agent clock minus the client clock, from the ping/pong estimate.
    pub fn on_presented(
        &mut self,
        window_id: u32,
        frame_id: u64,
        presented_us: u64,
        offset_us: i64,
    ) -> Option<FrameStages> {
        if !self.enabled {
            return None;
        }
        let window = self.windows.get_mut(&window_id)?;
        let partial = window.in_flight.remove(&frame_id)?;
        // A frame that was presented without a decode report is one the passthrough path
        // handled; treat arrival as the decode point rather than inventing a duration.
        let decoded_us = partial.decoded_us.unwrap_or(partial.arrived_us);

        let captured_client = agent_to_client(partial.captured_us, offset_us);
        let encoded_client = agent_to_client(partial.encoded_us, offset_us);

        let stages = FrameStages {
            capture_to_encode_us: partial.encoded_us.saturating_sub(partial.captured_us),
            encode_to_arrival_us: partial.arrived_us.saturating_sub(encoded_client),
            arrival_to_decode_us: decoded_us.saturating_sub(partial.arrived_us),
            decode_to_present_us: presented_us.saturating_sub(decoded_us),
            client_us: presented_us.saturating_sub(partial.arrived_us),
            total_us: presented_us.saturating_sub(captured_client),
        };

        window.capture_to_encode.push(stages.capture_to_encode_us);
        window.encode_to_arrival.push(stages.encode_to_arrival_us);
        window.arrival_to_decode.push(stages.arrival_to_decode_us);
        window.decode_to_present.push(stages.decode_to_present_us);
        window.client.push(stages.client_us);
        window.total.push(stages.total_us);
        window.presented += 1;
        Some(stages)
    }

    /// A frame was decoded to nothing, or failed. It will never be presented.
    pub fn on_dropped(&mut self, window_id: u32, frame_id: u64) {
        if !self.enabled {
            return;
        }
        if let Some(window) = self.windows.get_mut(&window_id) {
            if window.in_flight.remove(&frame_id).is_some() {
                window.dropped += 1;
            }
        }
    }

    /// Stop tracking a window.
    pub fn forget(&mut self, window_id: u32) {
        self.windows.remove(&window_id);
    }

    /// Whether any window has completed frames to report on.
    #[must_use]
    pub fn has_samples(&self) -> bool {
        self.windows.values().any(|w| !w.total.is_empty())
    }

    /// A human-readable report, one block per window.
    ///
    /// `rtt_us` and `offset_us` are printed alongside because they are what the reader needs in
    /// order to know how much of the end-to-end figure to believe.
    #[must_use]
    pub fn report(&self, rtt: Option<u64>, offset_us: Option<i64>) -> String {
        let mut out = String::new();
        out.push_str("oxclient latency: capture-to-present, microseconds\n");
        match (rtt, offset_us) {
            (Some(rtt), Some(offset)) => out.push_str(&format!(
                "  clock: round trip {rtt} us, agent offset {offset} us, \
                 so the cross-clock stages are good to about +/-{} us\n",
                rtt / 2
            )),
            _ => out.push_str(
                "  clock: no pong yet, so the agent's timestamps cannot be placed on this \
                 clock; only the client-only stages below are meaningful\n",
            ),
        }

        let mut windows: Vec<(&u32, &WindowLatency)> = self.windows.iter().collect();
        windows.sort_by_key(|(id, _)| **id);
        for (window_id, window) in windows {
            if window.total.is_empty() {
                continue;
            }
            out.push_str(&format!(
                "  window {window_id}: {} presented, {} never presented\n",
                window.presented, window.dropped
            ));
            for (label, samples, exact) in [
                ("capture->encode ", &window.capture_to_encode, true),
                ("encode->arrival ", &window.encode_to_arrival, false),
                ("arrival->decode ", &window.arrival_to_decode, true),
                ("decode->present ", &window.decode_to_present, true),
                ("client total    ", &window.client, true),
                ("END TO END      ", &window.total, false),
            ] {
                out.push_str(&format!(
                    "    {label} p50 {:>8}  p95 {:>8}  p99 {:>8}  max {:>8}{}\n",
                    show(samples.percentile(50)),
                    show(samples.percentile(95)),
                    show(samples.percentile(99)),
                    show(samples.max()),
                    if exact { "" } else { "   (+/- clock error)" }
                ));
            }
        }
        out
    }
}

impl Default for LatencyMonitor {
    fn default() -> Self {
        Self::disabled()
    }
}

fn show(value: Option<u64>) -> String {
    value.map_or_else(|| "-".to_string(), |v| v.to_string())
}

/// Moves an agent timestamp onto the client's clock.
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
    fn a_disabled_monitor_records_nothing() {
        let mut monitor = LatencyMonitor::disabled();
        monitor.on_arrival(1, 1, 0, 0, 0);
        monitor.on_decoded(1, 1, 0);

        assert_eq!(monitor.on_presented(1, 1, 100, 0), None);
        assert!(!monitor.has_samples());
    }

    #[test]
    fn the_stages_split_the_journey_and_add_up() {
        let mut monitor = LatencyMonitor::enabled();
        // The agent's clock reads 10_000 us ahead of the client's.
        // Captured at agent 15_000 (client 5_000), encoded at agent 15_400 (client 5_400).
        // Arrived on the client at 6_000, decoded at 9_000, presented at 9_500.
        monitor.on_arrival(1, 7, 15_000, 15_400, 6_000);
        monitor.on_decoded(1, 7, 9_000);

        let stages = monitor
            .on_presented(1, 7, 9_500, 10_000)
            .expect("the frame was seen arriving");

        assert_eq!(stages.capture_to_encode_us, 400);
        assert_eq!(
            stages.encode_to_arrival_us, 600,
            "client 6000 - client 5400"
        );
        assert_eq!(stages.arrival_to_decode_us, 3_000);
        assert_eq!(stages.decode_to_present_us, 500);
        assert_eq!(
            stages.client_us, 3_500,
            "arrival to present, no agent clock"
        );
        assert_eq!(stages.total_us, 4_500, "client 9500 - client 5000");
        // The four stages account for the whole of the end-to-end figure.
        assert_eq!(
            stages.capture_to_encode_us
                + stages.encode_to_arrival_us
                + stages.arrival_to_decode_us
                + stages.decode_to_present_us,
            stages.total_us
        );
    }

    #[test]
    fn the_client_only_span_does_not_move_with_the_clock_estimate() {
        // The point of separating them: a wrong offset must not corrupt what the client measured
        // entirely with its own clock.
        let mut monitor = LatencyMonitor::enabled();
        for (frame_id, offset) in [(1u64, 0i64), (2, 10_000), (3, -250_000)] {
            monitor.on_arrival(1, frame_id, 15_000, 15_400, 6_000);
            monitor.on_decoded(1, frame_id, 9_000);
            let stages = monitor
                .on_presented(1, frame_id, 9_500, offset)
                .expect("recorded");
            assert_eq!(stages.client_us, 3_500, "offset {offset} changed it");
            assert_eq!(stages.arrival_to_decode_us, 3_000);
            assert_eq!(stages.decode_to_present_us, 500);
            assert_eq!(stages.capture_to_encode_us, 400, "agent-only, also exact");
        }
    }

    #[test]
    fn an_agent_clock_behind_the_client_is_handled() {
        let mut monitor = LatencyMonitor::enabled();
        // Agent clock 2_000 us *behind*: agent 3_000 is client 5_000.
        monitor.on_arrival(1, 1, 3_000, 3_200, 5_400);
        monitor.on_decoded(1, 1, 5_500);

        let stages = monitor.on_presented(1, 1, 5_600, -2_000).expect("recorded");

        assert_eq!(
            stages.encode_to_arrival_us, 200,
            "client 5400 - client 5200"
        );
        assert_eq!(stages.total_us, 600, "client 5600 - client 5000");
    }

    #[test]
    fn a_frame_never_seen_arriving_is_not_invented() {
        let mut monitor = LatencyMonitor::enabled();
        assert_eq!(monitor.on_presented(1, 1, 100, 0), None);
        assert_eq!(monitor.on_presented(9, 9, 100, 0), None);
    }

    #[test]
    fn a_frame_presented_without_a_decode_report_still_accounts() {
        // The passthrough path presents without a separate decode step.
        let mut monitor = LatencyMonitor::enabled();
        monitor.on_arrival(1, 1, 1_000, 1_100, 2_000);

        let stages = monitor.on_presented(1, 1, 2_500, 0).expect("recorded");

        assert_eq!(stages.arrival_to_decode_us, 0);
        assert_eq!(stages.decode_to_present_us, 500);
        assert_eq!(stages.client_us, 500);
    }

    #[test]
    fn frames_that_never_complete_are_bounded_and_counted() {
        let mut monitor = LatencyMonitor::enabled();
        for frame_id in 0..(MAX_IN_FLIGHT as u64 + 50) {
            monitor.on_arrival(1, frame_id, 0, 0, 0);
        }

        let window = monitor.windows.get(&1).expect("window exists");
        assert!(window.in_flight.len() <= MAX_IN_FLIGHT);
        assert_eq!(window.dropped, 50, "evicted frames are counted, not lost");
    }

    #[test]
    fn dropped_frames_are_counted_rather_than_left_in_flight() {
        let mut monitor = LatencyMonitor::enabled();
        monitor.on_arrival(1, 1, 0, 0, 0);
        monitor.on_dropped(1, 1);

        let window = monitor.windows.get(&1).expect("window exists");
        assert_eq!(window.dropped, 1);
        assert!(window.in_flight.is_empty());
        // And it cannot then be presented.
        assert_eq!(monitor.on_presented(1, 1, 100, 0), None);
    }

    #[test]
    fn windows_are_accounted_separately() {
        let mut monitor = LatencyMonitor::enabled();
        monitor.on_arrival(1, 1, 0, 0, 0);
        monitor.on_decoded(1, 1, 100);
        monitor.on_presented(1, 1, 200, 0);
        monitor.on_arrival(2, 1, 0, 0, 0);
        monitor.on_decoded(2, 1, 900);
        monitor.on_presented(2, 1, 1_000, 0);

        assert_eq!(monitor.windows[&1].total.last(), Some(200));
        assert_eq!(monitor.windows[&2].total.last(), Some(1_000));

        monitor.forget(1);
        assert!(!monitor.windows.contains_key(&1));
        assert!(monitor.windows.contains_key(&2));
    }

    #[test]
    fn the_report_says_when_the_clock_is_unknown() {
        let mut monitor = LatencyMonitor::enabled();
        monitor.on_arrival(1, 1, 0, 0, 0);
        monitor.on_decoded(1, 1, 100);
        monitor.on_presented(1, 1, 200, 0);

        let without = monitor.report(None, None);
        assert!(without.contains("no pong yet"), "{without}");

        let with = monitor.report(Some(800), Some(4_000));
        assert!(with.contains("round trip 800"), "{with}");
        assert!(with.contains("+/-400"), "half the round trip: {with}");
        assert!(with.contains("window 1"), "{with}");
        assert!(with.contains("END TO END"), "{with}");
    }

    #[test]
    fn percentiles_come_from_the_samples_not_the_mean() {
        let mut monitor = LatencyMonitor::enabled();
        // Ninety-nine fast frames and one very slow one: the mean would hide it, p99 must not.
        for frame_id in 0..99 {
            monitor.on_arrival(1, frame_id, 0, 0, 0);
            monitor.on_decoded(1, frame_id, 0);
            monitor.on_presented(1, frame_id, 1_000, 0);
        }
        monitor.on_arrival(1, 99, 0, 0, 0);
        monitor.on_decoded(1, 99, 0);
        monitor.on_presented(1, 99, 500_000, 0);

        let window = &monitor.windows[&1];
        assert_eq!(window.total.percentile(50), Some(1_000));
        assert_eq!(window.total.max(), Some(500_000));
        let report = monitor.report(Some(100), Some(0));
        assert!(
            report.contains("500000"),
            "the outlier must appear: {report}"
        );
    }
}
