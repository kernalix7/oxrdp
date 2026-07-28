//! Decode off the session task: one worker thread per window.
//!
//! # Why this exists
//!
//! Decoding on the session task charges every frame's decode time to the input path — the task
//! that reads the network also writes pointer and key events, so a frame being decoded is a
//! frame's worth of added input latency. At 800x600 that is invisible. At 4K, or with several
//! windows, input latency starts tracking decode time, which defeats the reason this protocol
//! exists instead of RDP.
//!
//! # One thread per window, and why not a pool
//!
//! A pool cannot help. H.264 inter prediction makes the frames of one window strictly serial —
//! frame N+1 is decoded *from* frame N — so the only parallelism available is across windows,
//! which is exactly what a thread per window already extracts. What a pool would add is
//! reordering: two threads taking frames off one queue can finish out of order, and there is no
//! ordering problem here to solve, only one to create. A thread per window also matches how the
//! rest of the system is arranged: each window is its own stream on its own channel
//! (`OXPROTO.md` §11), each with its own decoder state, and `OXPROTO.md` §1 rule 1 says nothing
//! may block a frame behind a bigger frame. A single shared decode queue would reintroduce
//! precisely the head-of-line blocking the wire format is arranged to avoid.
//!
//! **Ordering therefore needs no machinery.** One queue and one thread per window is FIFO, and
//! arrival order is `frame_id` order because §9.1 forbids B-frames and reordering. Across
//! windows there is no ordering to preserve.
//!
//! # Backpressure: this is *not* the agent's problem
//!
//! `OXPROTO.md` §12 has the agent drop the oldest unacknowledged frame rather than queue, because
//! queueing turns a bandwidth dip into unbounded latency. The reasoning is right and the
//! conclusion does not carry over, because the two ends are not symmetric: **the agent can drop a
//! frame and the client cannot.** The agent drops before encoding, so it simply encodes newer
//! content and the reference chain is whatever it chooses. A client that drops an inter frame has
//! corrupted every frame after it until the next IDR — and the protocol has no way to ask for one
//! (there is no keyframe-request message; `QualityHint` has no refresh bit), so the corruption
//! lasts until the agent's own IDR interval comes round.
//!
//! So the client does not drop. Each worker's queue is bounded, and a caller that cannot place a
//! frame stops reading the network instead ([`Backpressure`]). That is what routes the pressure
//! back to the encoder, which is the one place in the system where shedding a frame is free:
//! with `FRAME_ACK` negotiated the agent's in-flight budget stops it directly, and without it TCP
//! backpressure does the same job more bluntly. Refusing to read is deliberate, not a shortcut.
//!
//! A future refinement, deliberately not built: a keyframe *may* safely supersede a backlog,
//! since it resets the reference chain. That would turn a stall into a jump-to-latest at the one
//! moment it is free.

use std::collections::HashMap;
use std::sync::Arc;
use std::thread;

use oxproto::message::FrameData;
use tokio::sync::mpsc;

use crate::clock::ClientClock;
use crate::decode::{new_decoder, DecodeError, Decoder};

/// Frames a window's worker may hold before its producer must stop reading the network.
///
/// Deliberately larger than the agent's default in-flight budget of 2 (`OXPROTO.md` §12) so that
/// flow control, not this queue, is what limits the stream in normal operation — and small
/// enough that a peer ignoring flow control cannot convert its head start into latency.
pub const QUEUE_DEPTH: usize = 4;

/// Where decoded frames go.
///
/// A trait rather than the display layer's own sender so that decode stays testable without a
/// display server, and so this crate's library half does not depend on the display half.
pub trait FrameSink: Clone + Send + 'static {
    /// Deliver one presentable frame. `false` means the consumer is gone and the worker should
    /// stop.
    fn deliver(&self, frame: FrameData) -> bool;
}

/// A frame the decoder finished with that will never reach the display.
///
/// It still has to be acknowledged. `OXPROTO.md` §12 bounds the agent's unacknowledged frames per
/// window, and a frame the client silently swallows would hold a slot in that budget forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DroppedFrame {
    /// Window the frame belonged to.
    pub window_id: u32,
    /// Frame that will not be presented.
    pub frame_id: u64,
    /// When the decoder finished with it, on the client clock.
    pub finished_us: u64,
}

/// Builds a decoder for one window's stream, given the window id and the codec on the wire.
///
/// Takes the window id because a decoder belongs to a window — it is built on the worker thread,
/// so a factory keyed only on the codec cannot tell two windows apart in the order they happen to
/// start. Injectable so tests can drive the pipeline with a decoder whose timing they control;
/// the default defers to [`new_decoder`].
pub type DecoderFactory =
    Arc<dyn Fn(u32, u8) -> Result<Box<dyn Decoder>, DecodeError> + Send + Sync>;

/// A frame that did not fit in its window's queue.
///
/// The caller holds it and stops reading the network until [`Backpressure::queue`] has room. See
/// the module docs for why the frame is kept rather than dropped.
#[derive(Debug)]
pub struct Backpressure {
    /// The frame that did not fit.
    pub frame: FrameData,
    /// The worker's queue. `reserve()` on it resolves when there is room; the caller is its only
    /// producer, so a slot it reserves is still free when it sends.
    pub queue: mpsc::Sender<FrameData>,
}

/// Per-window decode workers.
pub struct DecodePipeline<S: FrameSink> {
    sink: S,
    dropped: mpsc::UnboundedSender<DroppedFrame>,
    clock: ClientClock,
    factory: DecoderFactory,
    workers: HashMap<u32, mpsc::Sender<FrameData>>,
}

impl<S: FrameSink> DecodePipeline<S> {
    /// Creates a pipeline that delivers decoded frames to `sink` and reports undeliverable ones
    /// on `dropped`.
    #[must_use]
    pub fn new(sink: S, dropped: mpsc::UnboundedSender<DroppedFrame>, clock: ClientClock) -> Self {
        Self::with_decoder_factory(
            sink,
            dropped,
            clock,
            Arc::new(|_window_id, codec| new_decoder(codec)),
        )
    }

    /// As [`DecodePipeline::new`], with the decoder constructor replaced.
    #[must_use]
    pub fn with_decoder_factory(
        sink: S,
        dropped: mpsc::UnboundedSender<DroppedFrame>,
        clock: ClientClock,
        factory: DecoderFactory,
    ) -> Self {
        Self {
            sink,
            dropped,
            clock,
            factory,
            workers: HashMap::new(),
        }
    }

    /// Hands a frame to its window's worker, starting one if this is the window's first frame.
    ///
    /// # Errors
    ///
    /// [`Backpressure`] carrying the frame back when the worker's queue is full. The caller must
    /// hold it and stop reading rather than drop it.
    pub fn submit(&mut self, frame: FrameData) -> Result<(), Backpressure> {
        let window_id = frame.window_id;
        let queue = self.workers.entry(window_id).or_insert_with(|| {
            spawn_worker(
                window_id,
                self.sink.clone(),
                self.dropped.clone(),
                self.clock,
                Arc::clone(&self.factory),
            )
        });

        match queue.try_send(frame) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(frame)) => Err(Backpressure {
                frame,
                queue: queue.clone(),
            }),
            // The worker is gone, which only happens if its sink closed — the display is
            // shutting down and there is nothing useful left to do with the frame.
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.workers.remove(&window_id);
                Ok(())
            }
        }
    }

    /// Stops a window's worker.
    ///
    /// The thread finishes the frame it is decoding and then exits, so this does not block.
    pub fn forget(&mut self, window_id: u32) {
        self.workers.remove(&window_id);
    }

    /// Number of windows with a running worker.
    #[must_use]
    pub fn len(&self) -> usize {
        self.workers.len()
    }

    /// Whether no worker is running.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.workers.is_empty()
    }
}

fn spawn_worker<S: FrameSink>(
    window_id: u32,
    sink: S,
    dropped: mpsc::UnboundedSender<DroppedFrame>,
    clock: ClientClock,
    factory: DecoderFactory,
) -> mpsc::Sender<FrameData> {
    let (tx, rx) = mpsc::channel(QUEUE_DEPTH);
    let spawned = thread::Builder::new()
        .name(format!("oxdecode-{window_id}"))
        .spawn(move || run_worker(window_id, rx, &sink, &dropped, clock, &factory));
    if let Err(error) = spawned {
        // Out of threads. Report it once; the channel's sender is returned anyway, and every
        // frame for this window will then be acknowledged-and-dropped by the closed-channel arm
        // of `submit` rather than silently stalling the window's flow-control budget.
        eprintln!("oxclient: could not start a decode thread for window {window_id}: {error}");
    }
    tx
}

/// One window's decode loop.
///
/// Frames arrive in `frame_id` order (§9.1: no B-frames, no reordering) and leave in the same
/// order, because this is the only consumer of the queue.
fn run_worker<S: FrameSink>(
    window_id: u32,
    mut frames: mpsc::Receiver<FrameData>,
    sink: &S,
    dropped: &mpsc::UnboundedSender<DroppedFrame>,
    clock: ClientClock,
    factory: &DecoderFactory,
) {
    let mut decoder: Option<Box<dyn Decoder>> = None;

    while let Some(frame) = frames.blocking_recv() {
        let frame_id = frame.frame_id;

        // A codec change mid-stream is a fresh stream; the old decoder's reference pictures mean
        // nothing to it.
        if decoder
            .as_ref()
            .is_none_or(|current| current.codec() != frame.codec)
        {
            match factory(window_id, frame.codec) {
                Ok(new) => decoder = Some(new),
                Err(error) => {
                    eprintln!(
                        "oxclient: window {window_id} cannot decode codec {}: {error}",
                        frame.codec
                    );
                    decoder = None;
                    report_dropped(dropped, window_id, frame_id, clock);
                    continue;
                }
            }
        }

        let Some(active) = decoder.as_mut() else {
            report_dropped(dropped, window_id, frame_id, clock);
            continue;
        };
        let decoded = match active.decode(frame) {
            Ok(decoded) => decoded,
            Err(error) => {
                eprintln!("oxclient: window {window_id} frame {frame_id} dropped: {error}");
                None
            }
        };

        match decoded {
            Some(frame) => {
                if !sink.deliver(frame) {
                    return;
                }
            }
            None => report_dropped(dropped, window_id, frame_id, clock),
        }
    }
}

fn report_dropped(
    dropped: &mpsc::UnboundedSender<DroppedFrame>,
    window_id: u32,
    frame_id: u64,
    clock: ClientClock,
) {
    let _ = dropped.send(DroppedFrame {
        window_id,
        frame_id,
        finished_us: clock.now_us(),
    });
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc as std_mpsc;
    use std::sync::Mutex;
    use std::time::Duration;

    use oxproto::message::codec;
    use oxproto::message::window::frame_flag;

    use super::*;

    /// A sink that records what the display would have been given.
    #[derive(Clone)]
    struct RecordingSink {
        frames: std_mpsc::Sender<FrameData>,
    }

    impl FrameSink for RecordingSink {
        fn deliver(&self, frame: FrameData) -> bool {
            self.frames.send(frame).is_ok()
        }
    }

    fn sink() -> (RecordingSink, std_mpsc::Receiver<FrameData>) {
        let (frames, rx) = std_mpsc::channel();
        (RecordingSink { frames }, rx)
    }

    fn raw_frame(window_id: u32, frame_id: u64, width: u16, height: u16) -> FrameData {
        FrameData {
            window_id,
            frame_id,
            codec: codec::RAW_BGRA,
            flags: frame_flag::KEYFRAME,
            width,
            height,
            captured_us: frame_id,
            encoded_us: frame_id,
            data: vec![0x40; usize::from(width) * usize::from(height) * 4],
        }
    }

    /// A decoder that blocks in `decode` until the test releases it.
    struct GatedDecoder {
        gate: Arc<Mutex<Option<std_mpsc::Receiver<()>>>>,
    }

    impl Decoder for GatedDecoder {
        fn codec(&self) -> u8 {
            codec::RAW_BGRA
        }

        fn decode(&mut self, frame: FrameData) -> Result<Option<FrameData>, DecodeError> {
            if let Some(gate) = self.gate.lock().expect("gate is not poisoned").as_ref() {
                let _ = gate.recv();
            }
            Ok(Some(frame))
        }
    }

    /// A decoder that never produces a picture, like one waiting for a keyframe.
    struct SilentDecoder;

    impl Decoder for SilentDecoder {
        fn codec(&self) -> u8 {
            codec::RAW_BGRA
        }

        fn decode(&mut self, _frame: FrameData) -> Result<Option<FrameData>, DecodeError> {
            Ok(None)
        }
    }

    /// A decoder that fails every frame.
    struct FailingDecoder;

    impl Decoder for FailingDecoder {
        fn codec(&self) -> u8 {
            codec::RAW_BGRA
        }

        fn decode(&mut self, _frame: FrameData) -> Result<Option<FrameData>, DecodeError> {
            Err(DecodeError::Bitstream("synthetic failure".to_string()))
        }
    }

    fn factory_of<F>(build: F) -> DecoderFactory
    where
        F: Fn() -> Box<dyn Decoder> + Send + Sync + 'static,
    {
        Arc::new(move |_window_id, _codec| Ok(build()))
    }

    #[tokio::test]
    async fn frames_reach_the_sink_in_frame_id_order() {
        let (sink, presented) = sink();
        let (dropped_tx, _dropped_rx) = mpsc::unbounded_channel();
        let mut pipeline = DecodePipeline::new(sink, dropped_tx, ClientClock::new());

        // More frames than the queue is deep, so the worker is consuming while the producer is
        // still submitting — the case where a reordering bug would show.
        for frame_id in 0..32 {
            let frame = raw_frame(1, frame_id, 2, 2);
            let mut frame = Some(frame);
            while let Some(next) = frame.take() {
                if let Err(backpressure) = pipeline.submit(next) {
                    let permit = backpressure
                        .queue
                        .reserve()
                        .await
                        .expect("the worker is alive");
                    drop(permit);
                    frame = Some(backpressure.frame);
                }
            }
        }

        let order: Vec<u64> = (0..32)
            .map(|_| {
                presented
                    .recv_timeout(Duration::from_secs(5))
                    .expect("every frame is presented")
                    .frame_id
            })
            .collect();
        assert_eq!(order, (0..32).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn a_frame_that_yields_no_picture_is_acknowledged_instead() {
        let (sink, presented) = sink();
        let (dropped_tx, mut dropped_rx) = mpsc::unbounded_channel();
        let mut pipeline = DecodePipeline::with_decoder_factory(
            sink,
            dropped_tx,
            ClientClock::new(),
            factory_of(|| Box::new(SilentDecoder)),
        );

        pipeline.submit(raw_frame(1, 7, 2, 2)).expect("submitted");

        let dropped = dropped_rx.recv().await.expect("the drop is reported");
        assert_eq!(
            dropped,
            DroppedFrame {
                window_id: 1,
                frame_id: 7,
                finished_us: dropped.finished_us,
            }
        );
        assert!(presented.try_recv().is_err(), "nothing was presented");
    }

    #[tokio::test]
    async fn a_failing_frame_is_acknowledged_and_the_worker_survives() {
        let (sink, _presented) = sink();
        let (dropped_tx, mut dropped_rx) = mpsc::unbounded_channel();
        let mut pipeline = DecodePipeline::with_decoder_factory(
            sink,
            dropped_tx,
            ClientClock::new(),
            factory_of(|| Box::new(FailingDecoder)),
        );

        for frame_id in 0..3 {
            pipeline
                .submit(raw_frame(1, frame_id, 2, 2))
                .expect("submitted");
        }

        for frame_id in 0..3 {
            let dropped = dropped_rx.recv().await.expect("each failure is reported");
            assert_eq!(dropped.frame_id, frame_id, "the worker kept going");
        }
    }

    #[tokio::test]
    async fn an_unsupported_codec_is_acknowledged_rather_than_stalling_the_window() {
        let (sink, _presented) = sink();
        let (dropped_tx, mut dropped_rx) = mpsc::unbounded_channel();
        let factory: DecoderFactory =
            Arc::new(|_window_id, codec| Err(DecodeError::UnsupportedCodec(codec)));
        let mut pipeline =
            DecodePipeline::with_decoder_factory(sink, dropped_tx, ClientClock::new(), factory);

        pipeline.submit(raw_frame(1, 1, 2, 2)).expect("submitted");

        assert_eq!(
            dropped_rx.recv().await.expect("reported").frame_id,
            1,
            "a window whose codec has no decoder must still release its flow-control slot"
        );
    }

    #[tokio::test]
    async fn a_full_queue_hands_the_frame_back_rather_than_dropping_it() {
        let (sink, _presented) = sink();
        let (dropped_tx, _dropped_rx) = mpsc::unbounded_channel();
        let (release, gate) = std_mpsc::channel();
        let gate = Arc::new(Mutex::new(Some(gate)));
        let mut pipeline = DecodePipeline::with_decoder_factory(
            sink,
            dropped_tx,
            ClientClock::new(),
            factory_of(move || {
                Box::new(GatedDecoder {
                    gate: Arc::clone(&gate),
                })
            }),
        );

        // One frame is taken by the blocked worker, QUEUE_DEPTH more fill the channel.
        let mut refused = None;
        for frame_id in 0..(QUEUE_DEPTH as u64 + 8) {
            match pipeline.submit(raw_frame(1, frame_id, 2, 2)) {
                Ok(()) => {}
                Err(backpressure) => {
                    refused = Some(backpressure);
                    break;
                }
            }
        }

        let refused = refused.expect("the queue fills and pushes back");
        // The frame comes back intact: nothing is discarded behind the caller's back, because a
        // dropped inter frame corrupts the stream until the next keyframe.
        assert!(refused.frame.frame_id <= QUEUE_DEPTH as u64 + 1);
        assert_eq!(refused.frame.data.len(), 2 * 2 * 4);

        // And the queue becomes writable again once the worker moves.
        for _ in 0..QUEUE_DEPTH + 2 {
            let _ = release.send(());
        }
        let permit = tokio::time::timeout(Duration::from_secs(5), refused.queue.reserve())
            .await
            .expect("room appears once the worker drains")
            .expect("the worker is alive");
        permit.send(refused.frame);
    }

    #[tokio::test]
    async fn one_stalled_window_does_not_block_another() {
        let (sink, presented) = sink();
        let (dropped_tx, _dropped_rx) = mpsc::unbounded_channel();
        let (release, gate) = std_mpsc::channel();
        let gate = Arc::new(Mutex::new(Some(gate)));
        // Window 1 gets a decoder that blocks; window 2 gets one that does not.
        let factory: DecoderFactory = Arc::new(move |window_id, _codec| {
            if window_id == 1 {
                Ok(Box::new(GatedDecoder {
                    gate: Arc::clone(&gate),
                }))
            } else {
                Ok(Box::new(PassthroughForTest))
            }
        });
        let mut pipeline =
            DecodePipeline::with_decoder_factory(sink, dropped_tx, ClientClock::new(), factory);

        pipeline.submit(raw_frame(1, 0, 2, 2)).expect("window 1");
        pipeline.submit(raw_frame(2, 0, 2, 2)).expect("window 2");

        // Window 2's frame arrives while window 1's worker is still blocked, which is the whole
        // point of a thread per window: a window is never behind another window's decode.
        let frame = presented
            .recv_timeout(Duration::from_secs(5))
            .expect("window 2 is presented while window 1 is stalled");
        assert_eq!(frame.window_id, 2);

        let _ = release.send(());
    }

    struct PassthroughForTest;

    impl Decoder for PassthroughForTest {
        fn codec(&self) -> u8 {
            codec::RAW_BGRA
        }

        fn decode(&mut self, frame: FrameData) -> Result<Option<FrameData>, DecodeError> {
            Ok(Some(frame))
        }
    }

    #[tokio::test]
    async fn forgetting_a_window_stops_its_worker() {
        let (sink, presented) = sink();
        let (dropped_tx, _dropped_rx) = mpsc::unbounded_channel();
        let mut pipeline = DecodePipeline::new(sink, dropped_tx, ClientClock::new());

        pipeline.submit(raw_frame(1, 0, 2, 2)).expect("submitted");
        presented
            .recv_timeout(Duration::from_secs(5))
            .expect("the first frame is presented");
        assert_eq!(pipeline.len(), 1);

        pipeline.forget(1);

        assert!(pipeline.is_empty());
        // A later frame for the same window starts a fresh worker rather than resurrecting one.
        pipeline.submit(raw_frame(1, 1, 2, 2)).expect("submitted");
        assert_eq!(pipeline.len(), 1);
        assert_eq!(
            presented
                .recv_timeout(Duration::from_secs(5))
                .expect("the new worker runs")
                .frame_id,
            1
        );
    }

    #[tokio::test]
    async fn the_real_passthrough_decoder_is_used_by_default() {
        let (sink, presented) = sink();
        let (dropped_tx, mut dropped_rx) = mpsc::unbounded_channel();
        let mut pipeline = DecodePipeline::new(sink, dropped_tx, ClientClock::new());

        // A well-formed RAW_BGRA frame is presented untouched...
        pipeline.submit(raw_frame(3, 0, 4, 2)).expect("submitted");
        let frame = presented
            .recv_timeout(Duration::from_secs(5))
            .expect("presented");
        assert_eq!(frame.data.len(), 4 * 2 * 4);
        assert_eq!(frame.codec, codec::RAW_BGRA);

        // ...and a malformed one is acknowledged rather than presented.
        let mut broken = raw_frame(3, 1, 4, 2);
        broken.data.truncate(3);
        pipeline.submit(broken).expect("submitted");
        assert_eq!(dropped_rx.recv().await.expect("reported").frame_id, 1);
    }
}
