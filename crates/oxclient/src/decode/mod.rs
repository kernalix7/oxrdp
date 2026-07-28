//! Frame decode: turn a wire [`FrameData`] into pixels the presenter can blit.
//!
//! # Where this sits
//!
//! `oxdisplay`'s presenter accepts exactly one thing: `RAW_BGRA`, tightly packed, top-down,
//! which it blits with `memcpy` because it is byte-identical to softbuffer's `0x00RRGGBB` `u32`
//! on a little-endian host. Decode therefore happens *here*, on the session side, and what
//! reaches the display layer is always already-presentable pixels. That keeps the display layer
//! codec-agnostic and keeps the `memcpy` present intact.
//!
//! # The contract
//!
//! A [`Decoder`] consumes one wire frame and returns either a `RAW_BGRA` frame carrying the same
//! ids and timestamps, or `None` when the frame was legitimately consumed without producing a
//! picture — the mid-GOP case, where inter-coded frames arriving before the first keyframe are
//! dropped rather than decoded into garbage. Errors are per frame and never fatal: the caller
//! logs, acknowledges the frame so the agent's in-flight budget does not stall, and carries on.
//!
//! # What plugs in later
//!
//! A VAAPI/hardware decoder is a third implementation of [`Decoder`] and a third arm in
//! [`new_decoder`]. It needs nothing else from this module, provided it also lands its output in
//! the layout described above.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;

use oxproto::message::{codec, FrameData};

pub mod pipeline;
pub mod yuv;

#[cfg(feature = "h264")]
pub mod annexb;
#[cfg(feature = "h264")]
pub mod h264;

/// Largest decoded picture the client will materialise, in pixels.
///
/// A frame header is untrusted input and a decoded frame costs 4 bytes per pixel, so an
/// out-of-range geometry has to be refused *before* the allocation, not after. 16 Mpx is 64 MiB
/// of BGRA: comfortably above a 4K window (8.3 Mpx) and far below what a corrupt or hostile
/// header could otherwise ask for. `OXPROTO.md` §16 bounds the encoded payload at 32 MiB but
/// says nothing about the decoded size, so this bound is the client's own.
pub const MAX_DECODED_PIXELS: usize = 16 << 20;

/// Why a frame could not be turned into pixels.
///
/// Every variant is per frame. None of them means the session is over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// No decoder is compiled in for this codec id.
    UnsupportedCodec(u8),
    /// The decoder for this codec exists but could not be created.
    Init {
        /// Codec id that failed to initialise.
        codec: u8,
        /// Message from the codec implementation.
        detail: String,
    },
    /// Payload length does not match the frame's declared geometry.
    PayloadLength {
        /// Bytes the geometry implies.
        expected: usize,
        /// Bytes actually present.
        actual: usize,
    },
    /// Frame geometry is empty, or larger than [`MAX_DECODED_PIXELS`].
    Geometry {
        /// Declared width.
        width: usize,
        /// Declared height.
        height: usize,
    },
    /// The codec rejected this frame's bitstream. The stream resynchronises at the next keyframe.
    Bitstream(String),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedCodec(codec) => write!(f, "no decoder for codec {codec}"),
            Self::Init { codec, detail } => {
                write!(f, "codec {codec} failed to initialise: {detail}")
            }
            Self::PayloadLength { expected, actual } => write!(
                f,
                "frame payload does not match its geometry: expected {expected} bytes, got {actual}"
            ),
            Self::Geometry { width, height } => {
                write!(f, "unusable frame geometry {width}x{height}")
            }
            Self::Bitstream(detail) => write!(f, "bitstream rejected: {detail}"),
        }
    }
}

impl Error for DecodeError {}

/// Turns wire frames of one codec into presentable `RAW_BGRA` frames.
///
/// Implementations are stateful and belong to a single window's stream: H.264 predicts from
/// previously decoded pictures, so feeding two windows through one decoder would be wrong.
/// [`WindowDecoders`] owns that fan-out.
///
/// `Send` is required because the session task is spawned onto the Tokio runtime, and because
/// moving decode off that task onto its own thread should stay a local change.
pub trait Decoder: Send {
    /// Codec id this decoder consumes.
    fn codec(&self) -> u8;

    /// Decode one wire frame.
    ///
    /// Returns the presentable frame, or `None` when the frame was consumed without producing a
    /// picture — dropped while waiting for a keyframe, or swallowed as parameter sets.
    ///
    /// # Errors
    ///
    /// Returns [`DecodeError`] if this frame could not be decoded. The decoder stays usable; a
    /// stream that errored resynchronises at the next keyframe.
    fn decode(&mut self, frame: FrameData) -> Result<Option<FrameData>, DecodeError>;
}

/// The `RAW_BGRA` decoder: validation, then the frame itself.
///
/// This is the default and it must stay that way. `RAW_BGRA` is the only end-to-end path that
/// has been validated against a real guest, and it is the fallback when the agent has no
/// encoder. Passing the frame through by value keeps it a zero-copy path.
#[derive(Debug, Default, Clone, Copy)]
pub struct PassthroughDecoder;

impl PassthroughDecoder {
    /// Creates a passthrough decoder.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Decoder for PassthroughDecoder {
    fn codec(&self) -> u8 {
        codec::RAW_BGRA
    }

    fn decode(&mut self, frame: FrameData) -> Result<Option<FrameData>, DecodeError> {
        let (width, height) =
            checked_geometry(usize::from(frame.width), usize::from(frame.height))?;
        // Checked here rather than left to the presenter so that "what leaves the decoder is
        // presentable" holds for every codec, and so the headless path catches it too.
        let expected = width * height * 4;
        if frame.data.len() != expected {
            return Err(DecodeError::PayloadLength {
                expected,
                actual: frame.data.len(),
            });
        }
        Ok(Some(frame))
    }
}

/// Validates a decoded picture's geometry against [`MAX_DECODED_PIXELS`].
fn checked_geometry(width: usize, height: usize) -> Result<(usize, usize), DecodeError> {
    let too_big = width
        .checked_mul(height)
        .is_none_or(|pixels| pixels > MAX_DECODED_PIXELS);
    if width == 0
        || height == 0
        || width > usize::from(u16::MAX)
        || height > usize::from(u16::MAX)
        || too_big
    {
        return Err(DecodeError::Geometry { width, height });
    }
    Ok((width, height))
}

/// Codec ids the client can actually decode, in descending preference, for `ClientHello`.
///
/// H.264 leads when it is compiled in: `RAW_BGRA` costs ~470 Mbit/s for one 800x600 window and
/// exists as a fallback, not a choice. With the `h264` feature off the list is exactly what the
/// bring-up client advertised, so the validated path is unaffected.
#[must_use]
pub fn preferred_codecs() -> Vec<u8> {
    #[cfg(feature = "h264")]
    {
        vec![codec::H264, codec::RAW_BGRA]
    }
    #[cfg(not(feature = "h264"))]
    {
        vec![codec::RAW_BGRA]
    }
}

/// Whether this build can decode `codec`.
///
/// The agent picks from what the client advertised, so a `ServerHello` naming anything else is a
/// negotiation violation the client should refuse loudly rather than render garbage for.
#[must_use]
pub fn supports_codec(codec: u8) -> bool {
    preferred_codecs().contains(&codec)
}

/// Creates a decoder for `codec`.
///
/// # Errors
///
/// [`DecodeError::UnsupportedCodec`] if this build has no decoder for the id, or
/// [`DecodeError::Init`] if the codec implementation refused to start.
pub fn new_decoder(codec: u8) -> Result<Box<dyn Decoder>, DecodeError> {
    match codec {
        codec::RAW_BGRA => Ok(Box::new(PassthroughDecoder::new())),
        #[cfg(feature = "h264")]
        codec::H264 => Ok(Box::new(h264::H264Decoder::new()?)),
        other => Err(DecodeError::UnsupportedCodec(other)),
    }
}

/// One decoder per window, created on that window's first frame.
///
/// Each window is its own encoded stream on its own channel (`OXPROTO.md` §11), so each needs
/// its own decoder state.
#[derive(Default)]
pub struct WindowDecoders {
    decoders: HashMap<u32, Box<dyn Decoder>>,
}

impl fmt::Debug for WindowDecoders {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WindowDecoders")
            .field("windows", &self.decoders.len())
            .finish()
    }
}

impl WindowDecoders {
    /// Creates an empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Routes one frame to its window's decoder, creating the decoder if this is the first frame.
    ///
    /// # Errors
    ///
    /// Returns [`DecodeError`] if the frame could not be decoded. The decoder is kept, because a
    /// stream that produced one bad frame recovers at its next keyframe.
    pub fn decode(&mut self, frame: FrameData) -> Result<Option<FrameData>, DecodeError> {
        let window_id = frame.window_id;
        let wire_codec = frame.codec;
        // A codec change mid-stream means a fresh stream; the old decoder's reference pictures
        // are meaningless for it.
        let stale = self
            .decoders
            .get(&window_id)
            .is_none_or(|decoder| decoder.codec() != wire_codec);
        if stale {
            self.decoders.insert(window_id, new_decoder(wire_codec)?);
        }
        self.decoders
            .get_mut(&window_id)
            .expect("the decoder was just created if it was missing")
            .decode(frame)
    }

    /// Drops a window's decoder, freeing its reference pictures.
    pub fn forget(&mut self, window_id: u32) {
        self.decoders.remove(&window_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn bgra_frame(window_id: u32, width: u16, height: u16) -> FrameData {
        FrameData {
            window_id,
            frame_id: 1,
            codec: codec::RAW_BGRA,
            flags: oxproto::message::window::frame_flag::KEYFRAME,
            width,
            height,
            captured_us: 10,
            encoded_us: 20,
            data: vec![0x7f; usize::from(width) * usize::from(height) * 4],
        }
    }

    #[test]
    fn passthrough_returns_the_frame_untouched() {
        let frame = bgra_frame(1, 4, 2);
        let expected = frame.clone();

        let decoded = PassthroughDecoder::new()
            .decode(frame)
            .expect("a well-formed RAW_BGRA frame decodes")
            .expect("and yields a picture");

        assert_eq!(decoded, expected);
    }

    #[test]
    fn passthrough_rejects_a_payload_that_does_not_match_the_geometry() {
        let mut frame = bgra_frame(1, 4, 2);
        frame.data.truncate(7);

        let error = PassthroughDecoder::new()
            .decode(frame)
            .expect_err("a short payload is rejected");

        assert_eq!(
            error,
            DecodeError::PayloadLength {
                expected: 32,
                actual: 7
            }
        );
    }

    #[test]
    fn passthrough_rejects_empty_geometry() {
        let mut frame = bgra_frame(1, 4, 2);
        frame.width = 0;
        frame.data.clear();

        assert_eq!(
            PassthroughDecoder::new().decode(frame),
            Err(DecodeError::Geometry {
                width: 0,
                height: 2
            })
        );
    }

    #[test]
    fn geometry_is_bounded_before_anything_is_allocated() {
        assert!(checked_geometry(MAX_DECODED_PIXELS + 1, 1).is_err());
        assert!(checked_geometry(usize::MAX, usize::MAX).is_err());
        assert_eq!(checked_geometry(1920, 1080), Ok((1920, 1080)));
    }

    #[test]
    fn raw_bgra_is_always_advertised_and_supported() {
        let codecs = preferred_codecs();
        assert!(codecs.contains(&codec::RAW_BGRA));
        assert!(supports_codec(codec::RAW_BGRA));
        assert!(!supports_codec(codec::H265));
        assert!(!supports_codec(0));
    }

    #[test]
    #[cfg(feature = "h264")]
    fn h264_leads_the_preference_list_when_it_is_compiled_in() {
        assert_eq!(preferred_codecs(), vec![codec::H264, codec::RAW_BGRA]);
        assert!(supports_codec(codec::H264));
    }

    #[test]
    #[cfg(not(feature = "h264"))]
    fn only_raw_bgra_is_advertised_without_the_h264_feature() {
        assert_eq!(preferred_codecs(), vec![codec::RAW_BGRA]);
        assert!(!supports_codec(codec::H264));
    }

    #[test]
    fn unknown_codecs_have_no_decoder() {
        assert_eq!(new_decoder(0).err(), Some(DecodeError::UnsupportedCodec(0)));
        assert_eq!(
            new_decoder(codec::AV1).err(),
            Some(DecodeError::UnsupportedCodec(codec::AV1))
        );
    }

    #[test]
    fn window_decoders_keep_one_decoder_per_window() {
        let mut decoders = WindowDecoders::new();

        decoders.decode(bgra_frame(1, 2, 2)).expect("window 1");
        decoders.decode(bgra_frame(2, 2, 2)).expect("window 2");
        assert_eq!(decoders.decoders.len(), 2);

        decoders.forget(1);
        assert_eq!(decoders.decoders.len(), 1);
        assert!(decoders.decoders.contains_key(&2));
    }

    #[test]
    fn window_decoders_rebuild_when_a_window_changes_codec() {
        let mut decoders = WindowDecoders::new();
        decoders.decode(bgra_frame(1, 2, 2)).expect("first codec");

        let mut switched = bgra_frame(1, 2, 2);
        switched.codec = codec::H265;
        let error = decoders
            .decode(switched)
            .expect_err("an unsupported codec has no decoder");

        assert_eq!(error, DecodeError::UnsupportedCodec(codec::H265));
    }

    #[test]
    fn a_bad_frame_does_not_disturb_other_windows() {
        let mut decoders = WindowDecoders::new();
        decoders.decode(bgra_frame(1, 2, 2)).expect("window 1");

        let mut broken = bgra_frame(2, 2, 2);
        broken.data.clear();
        assert!(decoders.decode(broken).is_err());

        // Window 1 still decodes, and window 2 recovers on its next good frame.
        assert!(decoders.decode(bgra_frame(1, 2, 2)).is_ok());
        assert!(decoders.decode(bgra_frame(2, 2, 2)).is_ok());
    }
}
