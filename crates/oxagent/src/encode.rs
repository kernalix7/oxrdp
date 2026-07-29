//! What the session driver needs from the platform to turn a captured window into a codec
//! bitstream (`docs/design/OXPROTO.md` §9.1).
//!
//! Mirrors [`crate::serve::WindowSource`] and [`crate::input::InputSink`]: the platform sits
//! behind a trait so `crate::serve::pump_frames`'s pipeline logic — which window needs a forced
//! keyframe and when, how `RAW_BGRA` and `H264` sessions differ, what happens when nothing is
//! ready yet — is exercised on the Linux build host with a fake encoder, and only
//! `crate::win::encode::WinFrameEncoder`'s Media Foundation plumbing is Windows-only.

use crate::serve::SourceFrame;

/// One encoded access unit, ready to become `FrameData.data` (`OXPROTO.md` §9.1: exactly one
/// access unit per `FrameData`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedFrame {
    /// Annex-B bytes: every NAL prefixed with a 4-byte start code, in the order §9.1 specifies.
    pub data: Vec<u8>,
    /// Whether this access unit is an IDR — `frame_flag::KEYFRAME`'s exact meaning for H.264,
    /// never "any I-frame" (§9.1).
    pub keyframe: bool,
    /// The coded picture size actually written into `data` — not necessarily the captured
    /// frame's own size. NV12 needs even dimensions; an encoder that padded an odd capture to
    /// the next even size must report the padded size here, because `FrameData.width`/`height`
    /// has to describe what the bitstream (and its SPS) actually says, not what was captured,
    /// or a decoder sizes its output surface wrong.
    pub width: u16,
    /// See `width`.
    pub height: u16,
}

/// Encodes captured frames into a codec bitstream, per window.
///
/// Submission and polling are separate calls rather than one blocking "encode and return"
/// method because a real hardware encoder is asynchronous under the hood — the encoded bytes
/// for a submitted frame do not necessarily come back before the next tick, and forcing that
/// into a synchronous call would mean either blocking the driver loop (unacceptable: the loop's
/// tick is the pacing clock for every window, not just this one) or hiding a queue inside the
/// implementation where the driver could not reason about it. This shape lets
/// `crate::serve::pump_frames` submit fresh input and drain whatever is ready in the same tick,
/// exactly like it already does for `WindowSource::next_frame`'s own non-blocking poll.
pub trait FrameEncoder {
    /// Submit a freshly captured frame for encoding. Non-blocking: does not wait for, or
    /// guarantee, an encoded result from this call. If the encoder cannot accept more input
    /// right now (it is still working on a previous frame), the submission is simply dropped —
    /// the same "newest content wins over queueing" philosophy `crate::pacing::FrameBudget`
    /// already applies to the frames it hands out, applied one stage earlier.
    ///
    /// `force_keyframe` requests an IDR for this frame: used for a window's first frame in a
    /// session and after a resolution change (`OXPROTO.md` §9.1), both cases where a decoder
    /// has nothing to reference yet. A request is not a guarantee the *very next* poll returns
    /// one — an encoder may have frames already in flight — but the implementation must ensure
    /// one is produced.
    fn submit(&mut self, handle: isize, frame: &SourceFrame, force_keyframe: bool);

    /// The next encoded access unit ready for `handle`, or `None` if nothing is ready yet.
    /// Called every tick regardless of whether `submit` was just called, since a hardware
    /// encoder's output can lag its input by a frame or more.
    fn poll(&mut self, handle: isize) -> Option<EncodedFrame>;

    /// Drop encoder state for a window — called when it closes, so its stream context (and any
    /// GPU/hardware resources it holds) does not outlive the window.
    fn forget(&mut self, handle: isize);

    /// Whether `handle` has given up on this codec permanently at its current resolution — not
    /// "nothing ready this tick" (`poll` returning `None` already covers that), but "this window
    /// will not produce output from this encoder no matter what is submitted to it next". A
    /// resolution change gets a fresh attempt; the exact size that already failed does not retry
    /// forever.
    ///
    /// `crate::serve::pump_frames` falls back to sending this window uncoded (`RAW_BGRA`) once
    /// this is true, rather than silently sending nothing for it for the rest of the session — a
    /// real guest run showed encoder construction can fail for reasons out of this crate's
    /// control (the driver refusing a required media-type constraint, say), and codec selection
    /// today is negotiated once per *session*, not per window, so there is no other way for one
    /// misbehaving window not to take its whole session down with it.
    fn failed(&self, handle: isize) -> bool;
}
