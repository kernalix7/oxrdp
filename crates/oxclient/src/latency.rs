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

/// The agent's default unacknowledged-frame budget (`OXPROTO.md` §12).
///
/// Used only to label samples: at or above this, the agent was waiting on this client rather
/// than the other way round.
const AGENT_IN_FLIGHT_BUDGET: usize = 2;

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

/// A frame that has just been read off the wire.
#[derive(Debug, Clone, Copy)]
pub struct ArrivedFrame {
    /// Window it belongs to.
    pub window_id: u32,
    /// Frame id.
    pub frame_id: u64,
    /// Agent clock, when capture completed.
    pub captured_us: u64,
    /// Agent clock, when encoding completed.
    pub encoded_us: u64,
    /// Client clock, when this client read it off the wire.
    pub arrived_us: u64,
    /// Whether it is a keyframe — roughly a hundred times the size of a delta, so the obvious
    /// candidate whenever a transport-shaped tail appears.
    pub keyframe: bool,
    /// Encoded size in bytes.
    pub bytes: usize,
}

/// A frame seen but not yet finished.
#[derive(Debug, Clone, Copy)]
struct Partial {
    captured_us: u64,
    encoded_us: u64,
    arrived_us: u64,
    decoded_us: Option<u64>,
    keyframe: bool,
    /// How many frames this client had received but not yet presented when this one arrived.
    ///
    /// The agent may hold only `max_in_flight` unacknowledged frames (`OXPROTO.md` §12, default
    /// 2), and this client acknowledges at presentation. So when this is at the budget, the next
    /// frame's send was *waiting on this client*, and its encode-to-arrival contains that wait.
    /// Recording it separates "the network was slow" from "we were why the agent had not sent".
    backlog: usize,
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
    /// Encode-to-arrival split by what the frame was, and by whether this client was already
    /// holding the agent's whole in-flight budget when it turned up. Between them these say
    /// whether a transport-shaped tail is transmission time, or this client's own back-pressure
    /// showing up a frame later.
    keyframe_transit: Samples,
    delta_transit: Samples,
    backlogged_transit: Samples,
    clear_transit: Samples,
    keyframe_bytes: Samples,
    delta_bytes: Samples,
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
            keyframe_transit: Samples::new(SAMPLE_WINDOW),
            delta_transit: Samples::new(SAMPLE_WINDOW),
            backlogged_transit: Samples::new(SAMPLE_WINDOW),
            clear_transit: Samples::new(SAMPLE_WINDOW),
            keyframe_bytes: Samples::new(SAMPLE_WINDOW),
            delta_bytes: Samples::new(SAMPLE_WINDOW),
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
    /// How many times this client stopped reading the wire because a decode queue was full, and
    /// for how long in total. While it is stopped, frames sit unread in the socket and their
    /// measured encode-to-arrival grows — so a transport-shaped tail with a large figure here is
    /// this client's doing rather than the network's.
    read_stalls: u64,
    read_stalled_us: u64,
}

impl LatencyMonitor {
    /// A monitor that records nothing.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            windows: HashMap::new(),
            enabled: false,
            read_stalls: 0,
            read_stalled_us: 0,
        }
    }

    /// A monitor that records.
    #[must_use]
    pub fn enabled() -> Self {
        Self {
            windows: HashMap::new(),
            enabled: true,
            read_stalls: 0,
            read_stalled_us: 0,
        }
    }

    /// Whether anything is being recorded.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Records that this client stopped reading the wire, and for how long.
    pub fn on_read_stall(&mut self, stalled_us: u64) {
        if !self.enabled {
            return;
        }
        self.read_stalls += 1;
        self.read_stalled_us = self.read_stalled_us.saturating_add(stalled_us);
    }

    /// A frame has been read off the wire, carrying the agent's own two timestamps.
    pub fn on_arrival(&mut self, frame: ArrivedFrame) {
        if !self.enabled {
            return;
        }
        let window = self
            .windows
            .entry(frame.window_id)
            .or_insert_with(WindowLatency::new);
        let backlog = window.in_flight.len();
        if frame.keyframe {
            window.keyframe_bytes.push(frame.bytes as u64);
        } else {
            window.delta_bytes.push(frame.bytes as u64);
        }
        window.in_flight.insert(
            frame.frame_id,
            Partial {
                captured_us: frame.captured_us,
                encoded_us: frame.encoded_us,
                arrived_us: frame.arrived_us,
                decoded_us: None,
                keyframe: frame.keyframe,
                backlog,
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
        if partial.keyframe {
            window.keyframe_transit.push(stages.encode_to_arrival_us);
        } else {
            window.delta_transit.push(stages.encode_to_arrival_us);
        }
        // At or above the budget, the agent could not have sent this frame any sooner: it was
        // waiting for an acknowledgement this client had not yet produced.
        if partial.backlog >= AGENT_IN_FLIGHT_BUDGET {
            window.backlogged_transit.push(stages.encode_to_arrival_us);
        } else {
            window.clear_transit.push(stages.encode_to_arrival_us);
        }
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
    pub fn report(&self, error_bound_us: Option<u64>, offset_us: Option<i64>) -> String {
        let mut out = String::new();
        out.push_str("oxclient latency: capture-to-present, microseconds\n");
        match (error_bound_us, offset_us) {
            (Some(bound), Some(offset)) => out.push_str(&format!(
                "  clock: agent offset {offset} us, good to about +/-{bound} us \
                 (half the round trip of the exchange it came from)\n"
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

            // Everything below exists to answer one question: when encode-to-arrival has a tail,
            // is it the transport, or is it this client?
            out.push_str("    encode->arrival, by what the frame was:\n");
            for (label, transit, bytes) in [
                ("keyframe", &window.keyframe_transit, &window.keyframe_bytes),
                ("delta   ", &window.delta_transit, &window.delta_bytes),
            ] {
                if transit.is_empty() {
                    continue;
                }
                out.push_str(&format!(
                    "      {label} n={:<5} p50 {:>8}  p95 {:>8}  max {:>8}   median {} bytes\n",
                    transit.len(),
                    show(transit.percentile(50)),
                    show(transit.percentile(95)),
                    show(transit.max()),
                    show(bytes.percentile(50)),
                ));
            }

            out.push_str(
                "    encode->arrival, by whether the agent was waiting on us (§12 budget):\n",
            );
            for (label, transit) in [
                ("we were backlogged ", &window.backlogged_transit),
                ("agent was free     ", &window.clear_transit),
            ] {
                if transit.is_empty() {
                    continue;
                }
                out.push_str(&format!(
                    "      {label} n={:<5} p50 {:>8}  p95 {:>8}  max {:>8}\n",
                    transit.len(),
                    show(transit.percentile(50)),
                    show(transit.percentile(95)),
                    show(transit.max()),
                ));
            }
        }

        out.push_str(&format!(
            "  this client stopped reading the wire {} times, {} us in total\n",
            self.read_stalls, self.read_stalled_us
        ));
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

    /// Shorthand for the tests: a delta frame of a plausible size.
    fn arrived(
        window_id: u32,
        frame_id: u64,
        captured_us: u64,
        encoded_us: u64,
        arrived_us: u64,
    ) -> ArrivedFrame {
        ArrivedFrame {
            window_id,
            frame_id,
            captured_us,
            encoded_us,
            arrived_us,
            keyframe: false,
            bytes: 236,
        }
    }

    #[test]
    fn a_disabled_monitor_records_nothing() {
        let mut monitor = LatencyMonitor::disabled();
        monitor.on_arrival(arrived(1, 1, 0, 0, 0));
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
        monitor.on_arrival(arrived(1, 7, 15_000, 15_400, 6_000));
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
            monitor.on_arrival(arrived(1, frame_id, 15_000, 15_400, 6_000));
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
        monitor.on_arrival(arrived(1, 1, 3_000, 3_200, 5_400));
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
        monitor.on_arrival(arrived(1, 1, 1_000, 1_100, 2_000));

        let stages = monitor.on_presented(1, 1, 2_500, 0).expect("recorded");

        assert_eq!(stages.arrival_to_decode_us, 0);
        assert_eq!(stages.decode_to_present_us, 500);
        assert_eq!(stages.client_us, 500);
    }

    #[test]
    fn frames_that_never_complete_are_bounded_and_counted() {
        let mut monitor = LatencyMonitor::enabled();
        for frame_id in 0..(MAX_IN_FLIGHT as u64 + 50) {
            monitor.on_arrival(arrived(1, frame_id, 0, 0, 0));
        }

        let window = monitor.windows.get(&1).expect("window exists");
        assert!(window.in_flight.len() <= MAX_IN_FLIGHT);
        assert_eq!(window.dropped, 50, "evicted frames are counted, not lost");
    }

    #[test]
    fn dropped_frames_are_counted_rather_than_left_in_flight() {
        let mut monitor = LatencyMonitor::enabled();
        monitor.on_arrival(arrived(1, 1, 0, 0, 0));
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
        monitor.on_arrival(arrived(1, 1, 0, 0, 0));
        monitor.on_decoded(1, 1, 100);
        monitor.on_presented(1, 1, 200, 0);
        monitor.on_arrival(arrived(2, 1, 0, 0, 0));
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
        monitor.on_arrival(arrived(1, 1, 0, 0, 0));
        monitor.on_decoded(1, 1, 100);
        monitor.on_presented(1, 1, 200, 0);

        let without = monitor.report(None, None);
        assert!(without.contains("no pong yet"), "{without}");

        // The bound is taken from `ClockSync`, which knows which exchange the offset came
        // from — the caller does not compute it, precisely so it cannot be computed from a
        // different, more flattering sample.
        let with = monitor.report(Some(400), Some(4_000));
        assert!(with.contains("agent offset 4000"), "{with}");
        assert!(with.contains("+/-400"), "{with}");
        assert!(with.contains("window 1"), "{with}");
        assert!(with.contains("END TO END"), "{with}");
    }

    /// The split exists to tell a transmission-time tail from a flow-control one. This is that
    /// question posed as a test: keyframes slow, deltas fast, and the report saying so.
    #[test]
    fn the_transit_split_separates_keyframes_from_deltas() {
        let mut monitor = LatencyMonitor::enabled();
        for frame_id in 0..30u64 {
            let keyframe = frame_id % 10 == 0;
            // Keyframes are a hundred times the size and, in this scenario, forty times slower.
            let arrived = if keyframe { 40_000 } else { 1_000 };
            monitor.on_arrival(ArrivedFrame {
                window_id: 1,
                frame_id,
                captured_us: 0,
                encoded_us: 0,
                arrived_us: arrived,
                keyframe,
                bytes: if keyframe { 102_400 } else { 236 },
            });
            monitor.on_decoded(1, frame_id, arrived);
            monitor.on_presented(1, frame_id, arrived, 0);
        }

        let window = &monitor.windows[&1];
        assert_eq!(window.keyframe_transit.len(), 3);
        assert_eq!(window.delta_transit.len(), 27);
        assert_eq!(window.keyframe_transit.percentile(50), Some(40_000));
        assert_eq!(window.delta_transit.percentile(50), Some(1_000));
        assert_eq!(window.keyframe_bytes.percentile(50), Some(102_400));

        let report = monitor.report(Some(100), Some(0));
        assert!(report.contains("keyframe n=3"), "{report}");
        assert!(report.contains("delta    n=27"), "{report}");
        assert!(report.contains("102400 bytes"), "{report}");
    }

    /// The other half of the question: was the agent waiting on us? A frame that arrives while
    /// this client is already holding the agent's whole budget could not have come sooner.
    #[test]
    fn transit_is_split_by_whether_the_agent_was_waiting_on_us() {
        let mut monitor = LatencyMonitor::enabled();
        // Three frames arrive and none is presented, so the backlog climbs 0, 1, 2.
        for frame_id in 0..3u64 {
            monitor.on_arrival(arrived(1, frame_id, 0, 0, 1_000));
        }
        for frame_id in 0..3u64 {
            monitor.on_decoded(1, frame_id, 1_000);
            monitor.on_presented(1, frame_id, 2_000, 0);
        }

        let window = &monitor.windows[&1];
        assert_eq!(
            window.clear_transit.len(),
            2,
            "the first two arrived with the agent free to send"
        );
        assert_eq!(
            window.backlogged_transit.len(),
            1,
            "the third arrived while we already held the whole budget"
        );
    }

    #[test]
    fn the_client_reports_its_own_read_stalls() {
        // A measurement that cannot see its own observer effect is not much of a measurement:
        // while this client refuses to read, frames sit unread and their arrival looks late.
        let mut monitor = LatencyMonitor::enabled();
        monitor.on_read_stall(12_000);
        monitor.on_read_stall(8_000);

        let report = monitor.report(Some(100), Some(0));
        assert!(
            report.contains("stopped reading the wire 2 times, 20000 us"),
            "{report}"
        );
    }

    #[test]
    fn percentiles_come_from_the_samples_not_the_mean() {
        let mut monitor = LatencyMonitor::enabled();
        // Ninety-nine fast frames and one very slow one: the mean would hide it, p99 must not.
        for frame_id in 0..99 {
            monitor.on_arrival(arrived(1, frame_id, 0, 0, 0));
            monitor.on_decoded(1, frame_id, 0);
            monitor.on_presented(1, frame_id, 1_000, 0);
        }
        monitor.on_arrival(arrived(1, 99, 0, 0, 0));
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
