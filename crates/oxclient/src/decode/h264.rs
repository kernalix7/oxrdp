//! Software H.264 decode, via Cisco's `openh264`.
//!
//! This is the CPU fallback and the reference implementation: it runs everywhere, needs no
//! driver, and defines the behaviour a hardware decoder has to match. VAAPI decode is a later
//! milestone and slots in as another [`Decoder`], not as a special case inside this one.
//!
//! The payload format is `OXPROTO.md` §9.1: Annex-B, one access unit per `FrameData`, parameter
//! sets in-band on every keyframe and only there, `KEYFRAME` meaning IDR, no reordering. Two of
//! those are load-bearing here rather than merely descriptive. Because a keyframe always carries
//! its own SPS and PPS, this decoder needs no out-of-band configuration and recovery is always
//! just "wait for the next keyframe". Because there are no B-frames, there is no reorder buffer:
//! one access unit in, at most one picture out, in `frame_id` order.
//!
//! # Streams that do not start at the beginning
//!
//! A client can attach to a window mid-GOP, and a frame can be dropped in flight. Both leave the
//! decoder holding no reference picture, and handing it inter-coded data in that state produces
//! either an error or a plausible-looking smear of whatever memory the codec had. So this
//! decoder is explicitly gated: it discards frames until an IDR arrives, both after construction
//! and after any bitstream error.

use openh264::decoder::Decoder as OpenH264Decoder;
use openh264::formats::YUVSource;
use oxproto::message::{codec, FrameData};

use super::annexb;
use super::yuv::{i420_to_bgra, I420Planes};
use super::{checked_geometry, DecodeError, Decoder};

/// Software H.264 decoder for one window's stream.
pub struct H264Decoder {
    inner: OpenH264Decoder,
    /// Set until a frame that can start a GOP arrives; frames are dropped while it holds.
    awaiting_keyframe: bool,
    /// Whether the §9.1 parameter-set violation has already been reported for this stream.
    warned_about_parameter_sets: bool,
    /// Frames discarded since this decoder last held a usable reference picture.
    dropped_while_resyncing: u32,
    /// Whether the stream's profile has already been reported.
    reported_profile: bool,
}

impl H264Decoder {
    /// Creates a decoder.
    ///
    /// # Errors
    ///
    /// [`DecodeError::Init`] if openh264 refuses to start.
    pub fn new() -> Result<Self, DecodeError> {
        let inner = OpenH264Decoder::new().map_err(|error| DecodeError::Init {
            codec: codec::H264,
            detail: error.to_string(),
        })?;
        Ok(Self {
            inner,
            awaiting_keyframe: true,
            warned_about_parameter_sets: false,
            dropped_while_resyncing: 0,
            reported_profile: false,
        })
    }

    /// Whether the next inter-coded frame would be dropped for lack of a reference picture.
    #[must_use]
    pub fn awaiting_keyframe(&self) -> bool {
        self.awaiting_keyframe
    }

    /// Says what a rejected access unit actually contained.
    ///
    /// A codec error code says only that something was refused. What is needed to find the bug
    /// is the shape of the access unit that was refused — which NAL units, in what order, how
    /// big — because that is what shows whether the encoder sent something `OXPROTO.md` §9.1
    /// does not allow. Only on the rejection path, and at debug level, so a healthy stream stays
    /// silent.
    fn report_rejection(&mut self, frame: &FrameData) {
        log::debug!(
            "window {} frame {} rejected: {} bytes, keyframe={}, contents: {}",
            frame.window_id,
            frame.frame_id,
            frame.data.len(),
            frame.is_keyframe(),
            annexb::describe(&frame.data)
        );

        // Parameter sets on a non-keyframe are forbidden by §9.1, and an encoder that emits them
        // will keep doing it, so this is worth saying out loud rather than leaving in a debug
        // log. Once per decoder: a periodic fault would otherwise repeat forever.
        if !frame.is_keyframe()
            && !self.warned_about_parameter_sets
            && annexb::has_parameter_sets(&frame.data)
        {
            self.warned_about_parameter_sets = true;
            log::warn!(
                "window {} frame {} carries parameter sets on a non-keyframe, which \
                 OXPROTO.md §9.1 forbids: {}",
                frame.window_id,
                frame.frame_id,
                annexb::describe(&frame.data)
            );
        }

        // A frame whose slices are all disposable is a coding-structure finding, not a
        // corruption one — temporal layers, or a profile beyond what this decoder implements.
        // Naming it is the difference between reading one log line and hexdumping the payload.
        if annexb::slices_are_non_reference(&frame.data) == Some(true) {
            log::debug!(
                "window {} frame {} is a non-reference picture (every slice has nal_ref_idc=0): \
                 nothing refers to it, so this is a coding-structure question rather than a \
                 corrupt bitstream",
                frame.window_id,
                frame.frame_id
            );
        }

        dump_rejected(frame);
    }

    /// Reports the profile the stream is coded in, once, from its first parameter set.
    ///
    /// This decoder implements what Cisco's openh264 implements, which is Constrained Baseline.
    /// A stream above that boundary can decode for a long time and then fail on whichever frame
    /// first uses a tool the decoder does not have — intermittent-looking, but systematic. One
    /// line at the start of a stream settles which of those is happening, so it is a warning
    /// when it is out of range and a debug note when it is not.
    fn report_profile(&mut self, frame: &FrameData) {
        if self.reported_profile {
            return;
        }
        let Some(profile) = annexb::stream_profile(&frame.data) else {
            return;
        };
        self.reported_profile = true;

        if profile.is_constrained_baseline() {
            log::debug!(
                "window {} stream is {} (profile_idc={}, constraints={:#04x}, level {}), which \
                 this decoder implements",
                frame.window_id,
                profile.name(),
                profile.profile_idc,
                profile.constraints,
                profile.level()
            );
        } else {
            log::warn!(
                "window {} stream is {} (profile_idc={}, constraints={:#04x}, level {}), but \
                 openh264 decodes Constrained Baseline only — frames using anything beyond it \
                 will be rejected however well the rest of the stream decodes",
                frame.window_id,
                profile.name(),
                profile.profile_idc,
                profile.constraints,
                profile.level()
            );
        }
    }
}

impl Decoder for H264Decoder {
    fn codec(&self) -> u8 {
        codec::H264
    }

    fn decode(&mut self, frame: FrameData) -> Result<Option<FrameData>, DecodeError> {
        if self.awaiting_keyframe {
            // `KEYFRAME` means IDR, not "any intra frame" (OXPROTO.md §9.1), and an IDR is the
            // only thing safe to start from: it carries its own parameter sets and nothing after
            // it may depend on anything before it. The flag is the agent's claim and the NAL
            // headers are the bitstream's own answer; either is enough, so a mislabelled
            // keyframe does not strand the window, but neither is relaxed to "intra".
            if !(frame.is_keyframe() || annexb::contains_idr(&frame.data)) {
                self.dropped_while_resyncing = self.dropped_while_resyncing.saturating_add(1);
                return Ok(None);
            }
            // What a rejection actually costs is not one frame — it is every frame until the
            // agent's next keyframe, because nothing in between can be decoded without the
            // reference picture that was lost. A periodic encoder fault therefore does far more
            // visible damage than its period suggests, and that is worth saying rather than
            // leaving to be inferred from a window that looks frozen.
            if self.dropped_while_resyncing > 0 {
                log::debug!(
                    "window {} resynchronised at frame {} after discarding {} frames",
                    frame.window_id,
                    frame.frame_id,
                    self.dropped_while_resyncing
                );
                self.dropped_while_resyncing = 0;
            }
            self.awaiting_keyframe = false;
        }
        self.report_profile(&frame);

        // Scoped so the borrow of `self.inner` that `decode` hands out ends before the error
        // path below touches `self` again.
        let picture = {
            match self.inner.decode(&frame.data) {
                // Parameter sets, or a frame the decoder is still accumulating: not an error,
                // just nothing to show yet.
                Ok(None) => Ok(None),
                Ok(Some(yuv)) => {
                    let (width, height) = yuv.dimensions();
                    let (y_stride, u_stride, v_stride) = yuv.strides();
                    checked_geometry(width, height).and_then(|_| {
                        i420_to_bgra(I420Planes {
                            y: yuv.y(),
                            u: yuv.u(),
                            v: yuv.v(),
                            width,
                            height,
                            y_stride,
                            u_stride,
                            v_stride,
                        })
                        .map(|bgra| Some((width, height, bgra)))
                        .ok_or_else(|| {
                            DecodeError::Bitstream(
                                "decoded picture is smaller than its declared geometry".to_string(),
                            )
                        })
                    })
                }
                Err(error) => Err(DecodeError::Bitstream(error.to_string())),
            }
        };

        match picture {
            Ok(None) => Ok(None),
            Ok(Some((width, height, data))) => Ok(Some(FrameData {
                window_id: frame.window_id,
                frame_id: frame.frame_id,
                codec: codec::RAW_BGRA,
                flags: frame.flags,
                // The size of the picture that actually came out, which is the only size the
                // buffer below can be labelled with. OXPROTO.md §9.1 requires it to equal the
                // wire header's `width`/`height` — the active SPS and the header describe the
                // same coded picture — and forbids sizing from `WindowGeometry`, which rides a
                // different channel with no ordering guarantee against this one.
                //
                // This is also why there is no resolution-change machinery here: §9.1 lets the
                // size change only on a keyframe carrying fresh parameter sets, which is exactly
                // when the decoder reconfigures itself, so following the picture follows the
                // stream. `width` and `height` are `<= u16::MAX` because `checked_geometry` said
                // so.
                width: width as u16,
                height: height as u16,
                captured_us: frame.captured_us,
                encoded_us: frame.encoded_us,
                data,
            })),
            Err(error) => {
                self.report_rejection(&frame);
                // One bad frame is not a dead session: drop the reference chain and wait for the
                // agent's next keyframe rather than predicting from a picture we do not have.
                self.awaiting_keyframe = true;
                Err(error)
            }
        }
    }
}

/// Writes a rejected access unit to `$OXCLIENT_DUMP_REJECTED/window-<id>-frame-<id>.h264`.
///
/// The NAL summary says what the access unit was made of; this hands over the bytes themselves,
/// for when that is not enough and the question becomes what is *inside* a NAL rather than which
/// ones are present. Off unless the variable is set, and failures to write are ignored — a
/// debugging aid must never be able to make the session worse than the bug it is chasing.
fn dump_rejected(frame: &FrameData) {
    let Ok(directory) = std::env::var("OXCLIENT_DUMP_REJECTED") else {
        return;
    };
    let path = std::path::Path::new(&directory).join(format!(
        "window-{}-frame-{}.h264",
        frame.window_id, frame.frame_id
    ));
    match std::fs::write(&path, &frame.data) {
        Ok(()) => log::debug!("wrote the rejected access unit to {}", path.display()),
        Err(error) => log::debug!("could not write {}: {error}", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use openh264::encoder::{
        BitRate, Encoder, EncoderConfig, FrameRate, FrameType, QpRange, UsageType,
    };
    use openh264::formats::{BgraSliceU8, YUVBuffer};
    use openh264::OpenH264API;
    use oxproto::message::window::frame_flag;

    use super::*;

    /// One encoded frame plus the pixels it was made from.
    struct Encoded {
        keyframe: bool,
        data: Vec<u8>,
        source: Vec<u8>,
    }

    /// Four flat colour blocks with a vertical boundary that walks right by `phase * 4` pixels.
    ///
    /// Flat blocks with hard edges are deliberate: flat areas make the lossy round trip
    /// measurable (any systematic colour error shows up immediately), the hard edges are where
    /// 4:2:0 chroma subsampling does its damage, and the four colours are far apart in every
    /// channel, so a swapped or shifted channel cannot pass.
    fn source_bgra(width: usize, height: usize, phase: usize) -> Vec<u8> {
        // B, G, R per block.
        const BLOCKS: [[u8; 3]; 4] = [[40, 60, 200], [60, 200, 40], [200, 40, 60], [70, 180, 180]];
        let split_x = boundary_x(width, phase);
        let split_y = height / 2;
        let mut data = Vec::with_capacity(width * height * 4);
        for y in 0..height {
            for x in 0..width {
                let block = usize::from(y >= split_y) * 2 + usize::from(x >= split_x);
                let [b, g, r] = BLOCKS[block];
                data.extend_from_slice(&[b, g, r, 0xff]);
            }
        }
        data
    }

    fn boundary_x(width: usize, phase: usize) -> usize {
        (width / 4 + phase * 4).min(width)
    }

    fn encoder() -> Encoder {
        let config = EncoderConfig::new()
            .usage_type(UsageType::ScreenContentRealTime)
            .max_frame_rate(FrameRate::from_hz(30.0))
            // Far more bitrate than 120x88 flat blocks need, and a low QP ceiling: the test is
            // about the decode path being correct, not about how the encoder behaves when
            // starved, so quantisation noise is kept small enough to assert on pixels.
            .bitrate(BitRate::from_bps(50_000_000))
            .qp(QpRange::new(0, 20))
            // Rate control must not silently drop a frame, or the test would be asserting on a
            // different frame than it thinks.
            .skip_frames(false);
        Encoder::with_api_config(OpenH264API::from_source(), config).expect("encoder starts")
    }

    /// Encodes `frames` frames, forcing an IDR at each index in `force_idr_at`.
    fn encode_clip(
        width: usize,
        height: usize,
        frames: usize,
        force_idr_at: &[usize],
    ) -> Vec<Encoded> {
        let mut encoder = encoder();
        (0..frames)
            .map(|index| {
                if force_idr_at.contains(&index) {
                    encoder.force_intra_frame();
                }
                let source = source_bgra(width, height, index);
                let yuv = YUVBuffer::from_rgb_source(BgraSliceU8::new(&source, (width, height)));
                let bitstream = encoder.encode(&yuv).expect("frame encodes");
                Encoded {
                    keyframe: matches!(bitstream.frame_type(), FrameType::IDR),
                    data: bitstream.to_vec(),
                    source,
                }
            })
            .collect()
    }

    fn wire_frame(encoded: &Encoded, frame_id: u64, width: usize, height: usize) -> FrameData {
        FrameData {
            window_id: 7,
            frame_id,
            codec: codec::H264,
            flags: if encoded.keyframe {
                frame_flag::KEYFRAME
            } else {
                0
            },
            width: width as u16,
            height: height as u16,
            captured_us: 1_000 + frame_id,
            encoded_us: 2_000 + frame_id,
            data: encoded.data.clone(),
        }
    }

    /// Mean absolute colour error per channel, and the worst error away from a block boundary.
    ///
    /// The two numbers answer different questions. The mean catches a wrong colour matrix or a
    /// wrong range, which shifts every pixel a little. The interior maximum catches a wrong
    /// stride or a swapped channel, which shows up as a large error somewhere specific. Pixels
    /// within three of a colour boundary are excluded from the maximum because 4:2:0 puts one
    /// chroma sample across two pixels there, so a large error at an edge is the format working
    /// as designed, not a bug.
    fn compare(
        decoded: &[u8],
        source: &[u8],
        width: usize,
        height: usize,
        phase: usize,
    ) -> (f64, u8) {
        assert_eq!(decoded.len(), source.len());
        let split_x = boundary_x(width, phase);
        let split_y = height / 2;
        let mut total = 0u64;
        let mut counted = 0u64;
        let mut worst_interior = 0u8;

        for y in 0..height {
            for x in 0..width {
                let base = (y * width + x) * 4;
                assert_eq!(decoded[base + 3], 0xff, "alpha is opaque at ({x},{y})");
                let near_edge = x.abs_diff(split_x) < 3
                    || y.abs_diff(split_y) < 3
                    || x < 3
                    || y < 3
                    || x + 3 >= width
                    || y + 3 >= height;
                for channel in 0..3 {
                    let error = decoded[base + channel].abs_diff(source[base + channel]);
                    total += u64::from(error);
                    counted += 1;
                    if !near_edge {
                        worst_interior = worst_interior.max(error);
                    }
                }
            }
        }

        #[allow(clippy::cast_precision_loss)]
        let mean = total as f64 / counted as f64;
        (mean, worst_interior)
    }

    /// Asserts a decoded frame carries the wire frame's identity and the source's pixels.
    fn assert_matches_source(
        decoded: &FrameData,
        encoded: &Encoded,
        width: usize,
        height: usize,
        phase: usize,
    ) {
        assert_eq!(decoded.codec, codec::RAW_BGRA, "presentable codec");
        assert_eq!(
            (decoded.width, decoded.height),
            (width as u16, height as u16)
        );
        assert_eq!(decoded.data.len(), width * height * 4);

        let (mean, worst_interior) = compare(&decoded.data, &encoded.source, width, height, phase);
        assert!(
            mean < 3.0,
            "mean absolute error {mean:.2} is too high for a high-bitrate flat-colour clip"
        );
        assert!(
            worst_interior <= 12,
            "worst interior error {worst_interior} suggests a layout or stride bug, not quantisation"
        );
    }

    #[test]
    fn decodes_a_synthetic_clip_back_to_the_source_pixels() {
        // 120x88 on purpose: even, but not a multiple of the 16-pixel macroblock, so the decoder
        // hands back padded strides and a cropped picture size. That is the case a naive
        // `width * height` copy gets wrong.
        let (width, height) = (120, 88);
        let clip = encode_clip(width, height, 6, &[0]);
        assert!(clip[0].keyframe, "the first encoded frame is an IDR");

        let mut decoder = H264Decoder::new().expect("decoder starts");
        for (index, encoded) in clip.iter().enumerate() {
            let frame = wire_frame(encoded, index as u64, width, height);
            let decoded = decoder
                .decode(frame)
                .unwrap_or_else(|error| panic!("frame {index} decodes: {error}"))
                .unwrap_or_else(|| panic!("frame {index} yields a picture"));

            assert_eq!(decoded.frame_id, index as u64, "frame id survives decode");
            assert_eq!(decoded.window_id, 7);
            assert_eq!(decoded.captured_us, 1_000 + index as u64);
            assert_eq!(decoded.encoded_us, 2_000 + index as u64);
            assert_matches_source(&decoded, encoded, width, height, index);
        }
    }

    #[test]
    fn drops_inter_frames_until_the_first_keyframe() {
        let (width, height) = (96, 64);
        let clip = encode_clip(width, height, 8, &[0, 5]);
        assert!(clip[5].keyframe, "the encoder honoured the forced IDR");

        let mut decoder = H264Decoder::new().expect("decoder starts");
        // Join mid-GOP: everything from frame 1 up to the next IDR predicts from a picture this
        // decoder has never seen.
        for (index, encoded) in clip.iter().enumerate().take(5).skip(1) {
            assert!(!encoded.keyframe, "frame {index} is inter-coded");
            let dropped = decoder
                .decode(wire_frame(encoded, index as u64, width, height))
                .expect("dropping a pre-keyframe frame is not an error");
            assert!(dropped.is_none(), "frame {index} must not be presented");
            assert!(decoder.awaiting_keyframe());
        }

        let decoded = decoder
            .decode(wire_frame(&clip[5], 5, width, height))
            .expect("the keyframe decodes")
            .expect("and yields a picture");
        assert!(!decoder.awaiting_keyframe());
        assert_matches_source(&decoded, &clip[5], width, height, 5);
    }

    #[test]
    fn starts_on_an_idr_whose_keyframe_flag_is_missing() {
        let (width, height) = (96, 64);
        let clip = encode_clip(width, height, 1, &[0]);

        let mut frame = wire_frame(&clip[0], 0, width, height);
        frame.flags = 0; // the agent forgot to set it
        assert!(!frame.is_keyframe());

        let decoded = H264Decoder::new()
            .expect("decoder starts")
            .decode(frame)
            .expect("the bitstream says IDR even though the flag does not")
            .expect("and yields a picture");
        assert_matches_source(&decoded, &clip[0], width, height, 0);
    }

    #[test]
    fn a_rejected_frame_leaves_the_decoder_usable() {
        let (width, height) = (96, 64);
        let clip = encode_clip(width, height, 4, &[0, 2]);
        let mut decoder = H264Decoder::new().expect("decoder starts");

        // An IDR NAL header with nothing behind it that any parameter set describes. It passes
        // the keyframe gate — which is the point: the failure has to be survived downstream of
        // that gate, where a real corrupted frame would fail.
        let mut garbage = wire_frame(&clip[0], 0, width, height);
        garbage.data = vec![0, 0, 0, 1, 0x65, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77];
        let error = decoder
            .decode(garbage)
            .expect_err("a bitstream openh264 cannot parse is an error");
        assert!(
            matches!(error, DecodeError::Bitstream(_)),
            "unexpected error: {error}"
        );

        // The decoder dropped its reference chain rather than pretending it still has one.
        assert!(decoder.awaiting_keyframe());
        assert!(decoder
            .decode(wire_frame(&clip[1], 1, width, height))
            .expect("an inter frame after the error is dropped, not decoded")
            .is_none());

        // ...and the stream recovers on the agent's next keyframe.
        let decoded = decoder
            .decode(wire_frame(&clip[2], 2, width, height))
            .expect("the next keyframe decodes")
            .expect("and yields a picture");
        assert_matches_source(&decoded, &clip[2], width, height, 2);
    }

    #[test]
    fn follows_a_resolution_change_mid_stream() {
        let mut decoder = H264Decoder::new().expect("decoder starts");

        let first = encode_clip(120, 88, 2, &[0]);
        for (index, encoded) in first.iter().enumerate() {
            let decoded = decoder
                .decode(wire_frame(encoded, index as u64, 120, 88))
                .expect("decodes")
                .expect("yields a picture");
            assert_matches_source(&decoded, encoded, 120, 88, index);
        }

        // A resized window restarts the encoder, so the stream carries new parameter sets and a
        // new IDR. The decoded picture size, not the wire header, is what the presenter gets.
        let second = encode_clip(64, 48, 2, &[0]);
        for (index, encoded) in second.iter().enumerate() {
            let decoded = decoder
                .decode(wire_frame(encoded, 100 + index as u64, 64, 48))
                .expect("decodes after the resolution change")
                .expect("yields a picture");
            assert_eq!((decoded.width, decoded.height), (64, 48));
            assert_matches_source(&decoded, encoded, 64, 48, index);
        }
    }

    #[test]
    fn an_empty_payload_is_not_a_picture() {
        let mut decoder = H264Decoder::new().expect("decoder starts");
        let empty = FrameData {
            window_id: 7,
            frame_id: 0,
            codec: codec::H264,
            flags: frame_flag::KEYFRAME,
            width: 96,
            height: 64,
            captured_us: 0,
            encoded_us: 0,
            data: Vec::new(),
        };
        // Whether openh264 calls this an error or simply produces nothing, the session survives
        // it and no picture is invented.
        assert!(!matches!(decoder.decode(empty), Ok(Some(_))));
    }

    /// Rewrites every 4-byte start code as the 3-byte form.
    ///
    /// OXPROTO.md §9.1 has the agent emit `00 00 00 01`, but requires a decoder to accept
    /// `00 00 01` for any NAL unit, since that is what a general-purpose Annex-B demuxer hands
    /// over. This is the only transformation in these tests that rewrites a real bitstream.
    fn shorten_start_codes(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(data.len());
        let mut index = 0;
        while index < data.len() {
            if data[index..].starts_with(&[0, 0, 0, 1]) {
                out.extend_from_slice(&[0, 0, 1]);
                index += 4;
            } else {
                out.push(data[index]);
                index += 1;
            }
        }
        out
    }

    #[test]
    fn accepts_three_byte_start_codes() {
        let (width, height) = (96, 64);
        let clip = encode_clip(width, height, 2, &[0]);
        let mut decoder = H264Decoder::new().expect("decoder starts");

        for (index, encoded) in clip.iter().enumerate() {
            let mut frame = wire_frame(encoded, index as u64, width, height);
            frame.data = shorten_start_codes(&frame.data);
            assert!(
                frame.data.len() < encoded.data.len(),
                "the rewrite actually removed start-code bytes"
            );
            let decoded = decoder
                .decode(frame)
                .expect("a three-byte-start-code stream decodes")
                .expect("and yields a picture");
            assert_matches_source(&decoded, encoded, width, height, index);
        }
    }

    #[test]
    fn skips_nal_types_it_has_no_use_for() {
        let (width, height) = (96, 64);
        let clip = encode_clip(width, height, 1, &[0]);

        // An access unit delimiter and an SEI ahead of the parameter sets, and filler data after
        // the slice: all legal, all irrelevant to this decoder, and none of them an error
        // (OXPROTO.md §9.1). Bytes avoid `00 00` runs so nothing here looks like a start code.
        let mut padded = Vec::new();
        padded.extend_from_slice(&[0, 0, 0, 1, 0x09, 0x10]); // access unit delimiter
        padded.extend_from_slice(&[0, 0, 0, 1, 0x06, 0x05, 0x11]); // SEI, user data unregistered
        padded.extend_from_slice(&[0xaa; 17]); // 16-byte uuid plus one payload byte
        padded.push(0x80); // rbsp trailing bits
        padded.extend_from_slice(&clip[0].data);
        padded.extend_from_slice(&[0, 0, 0, 1, 0x0c, 0xff, 0x80]); // filler data

        let mut frame = wire_frame(&clip[0], 0, width, height);
        frame.data = padded;

        let decoded = H264Decoder::new()
            .expect("decoder starts")
            .decode(frame)
            .expect("unknown NAL types are skipped, not fatal")
            .expect("and the picture still comes out");
        assert_matches_source(&decoded, &clip[0], width, height, 0);
    }

    #[test]
    fn window_decoders_keep_two_h264_streams_apart() {
        let (width, height) = (96, 64);
        let clip = encode_clip(width, height, 3, &[0]);
        let mut decoders = super::super::WindowDecoders::new();

        // Two windows, same content, decoded through the boxed-trait routing the session uses.
        for window_id in [1u32, 2] {
            for (index, encoded) in clip.iter().enumerate() {
                let mut frame = wire_frame(encoded, index as u64, width, height);
                frame.window_id = window_id;
                let decoded = decoders
                    .decode(frame)
                    .expect("decodes")
                    .expect("yields a picture");
                assert_eq!(decoded.window_id, window_id);
                assert_matches_source(&decoded, encoded, width, height, index);
            }
        }

        // Closing a window really drops its decoder: the stream that follows has to start at a
        // keyframe again rather than predicting from the old window's reference pictures.
        decoders.forget(1);
        let mut inter = wire_frame(&clip[1], 1, width, height);
        inter.window_id = 1;
        assert!(decoders
            .decode(inter)
            .expect("an inter frame on a fresh decoder is dropped, not an error")
            .is_none());
    }
}
