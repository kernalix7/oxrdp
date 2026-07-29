//! Media Foundation H.264 encoding (`crate::encode::FrameEncoder`, Windows-only).
//!
//! Deliberate choices:
//! - **Hardware MFT, software MFT fallback, driven identically.** [`probe_h264_support`]
//!   enumerates a hardware encoder first (`MFT_ENUM_FLAG_HARDWARE`) and only falls back to the
//!   built-in software encoder (`MFT_ENUM_FLAG_SYNCMFT`) if none is found. Hardware encoder MFTs
//!   are typically asynchronous under the hood; rather than implement the full
//!   `IMFMediaEventGenerator` event loop (`METransformNeedInput`/`METransformHaveOutput`), this
//!   sets `MF_TRANSFORM_ASYNC_UNLOCK` on an async transform's attribute store and drives it with
//!   the same synchronous `ProcessInput`/`ProcessOutput` calls a sync MFT uses — a documented,
//!   standard technique for exactly this case, and it means one code path handles both kinds.
//! - **No B-frames, low-latency rate control, explicit per-frame keyframe requests.** Configured
//!   through `ICodecAPI` (`CODECAPI_AVEncMPVDefaultBPictureCount = 0`,
//!   `CODECAPI_AVEncCommonRateControlMode = LowDelayVBR`, `CODECAPI_AVLowLatencyMode = true`
//!   where the transform supports it) — required by `OXPROTO.md` §9.1 for the flow control in
//!   §12 to stay sound: dropping the oldest unacknowledged frame is only safe if no later
//!   picture can reference one that got skipped. **A real guest run proved this `ICodecAPI` call
//!   alone is not sufficient**: `SetValue` succeeded and the transform still emitted B-pictures
//!   (Main profile, which permits them). Left in place — it costs nothing and may matter to a
//!   different encoder — but the actual, effective mechanism against B-frames is the profile
//!   constraint on the output media type below, which the transform cannot silently ignore the
//!   way it can an `ICodecAPI` behavioural property.
//! - **Parameter sets are never trusted to "just be there."** Whatever the transform's raw
//!   output actually contains, `crate::h264::reframe` normalizes it into what §9.1 requires —
//!   see that module for why this is not redundant with configuring the transform correctly.
//! - **A resolution change tears the whole per-window encoder down and rebuilds it**, rather
//!   than trying to reconfigure a live MFT's media type mid-stream. Simpler, and it naturally
//!   produces the fresh SPS/PPS + keyframe §9.1 requires on a coded-size change, since a new
//!   transform always starts a fresh stream.
//! - **No periodic re-keying: `CODECAPI_AVEncMPVGOPSize` is pinned huge.** §9.1 names exactly
//!   two events that produce a keyframe and nothing else; left to its own defaults, an encoder
//!   MFT is free to insert a periodic sync point anyway. Kept as spec hygiene regardless of the
//!   next bullet's finding: it was this file's first, and as it turned out incomplete, attempt to
//!   explain a real guest run rejecting one access unit in every thirty. See `GOP_SIZE_FRAMES`.
//! - **Constrained Baseline is a media-type constraint, not only an `ICodecAPI` request — and
//!   this is confirmed, not theorized.** A guest run's own SPS (this crate parses and logs it —
//!   `crate::h264::sps_profile`) first caught this encoder emitting Main profile
//!   (`profile_idc = 77`) regardless of `CODECAPI_AVEncMPVProfile` having been set, which is
//!   exactly what let B-pictures (`nal_ref_idc == 0` on a non-IDR slice) into the stream and
//!   openh264's Constrained-Baseline-only decoder reject every one of them. Setting
//!   `MF_MT_MPEG2_PROFILE` (`= eAVEncH264VProfile_ConstrainedBase`) on the output media type
//!   instead — Media Foundation's actual attribute for H.264 profile signalling despite the
//!   legacy "MPEG2" name, and a `SetOutputType` negotiation the transform must accept or reject,
//!   not a behavioural hint it can silently disregard — fixed it: redeployed and verified on the
//!   same guest, the SPS now reads Constrained Baseline, `nal_ref_idc` is never `0` on a delta
//!   frame, and decode rejections went from double digits to zero. That guest run also confirmed
//!   `CODECAPI_AVEncMPVGOPSize` *and* `CODECAPI_AVEncMPVDefaultBPictureCount` were both accepted
//!   by `SetValue` and silently ignored — a media-type constraint is not a nicer-sounding version
//!   of the same request, it is a structurally different, unignorable one, which is why
//!   `MAX_REF_FRAMES` below is verified from the bitstream rather than trusted from
//!   `ICodecAPI::GetValue` either.
//! - **Reference frame count pinned to 1, verified from the SPS, not from the property.** A
//!   third `ICodecAPI` property on this encoder has now been demonstrated accepted-and-ignored
//!   (the profile one, above) and a fourth was never checked against the bitstream at all until
//!   `MAX_REF_FRAMES`. `crate::h264::sps_ref_frame_info` — an Exp-Golomb bitstream reader, not
//!   just the fixed-byte read `sps_profile` does — reads `max_num_ref_frames` straight out of a
//!   real keyframe's SPS, logged per frame alongside the `ICodecAPI` readback so the two can be
//!   compared directly rather than one being assumed to imply the other.
//!
//! **What this file's author could not verify without a live Windows guest**, stated plainly
//! rather than left implicit: whether `MF_TRANSFORM_ASYNC_UNLOCK` actually lets every hardware
//! encoder this might run against be driven synchronously (it is documented Microsoft guidance,
//! not this crate's invention, but "documented" and "true of the specific driver on the test
//! guest" are not the same claim); the `MFTEnumEx` output array's ownership handling in
//! [`first_activate`], which follows the COM ownership rules as documented but has had no COM
//! debugger anywhere near it; and whether `CODECAPI_AVEncVideoMaxNumRefFrame` fares any better
//! than the three properties already shown ignored on this same encoder — `MAX_REF_FRAMES`'s doc
//! says why the SPS reading is what actually settles it, but that reading has not yet come back
//! from a guest run. Two items a guest run has already surfaced and this crate has not yet acted
//! on, deliberately, one change at a time rather than batched: `CODECAPI_AVEncMPVGOPSize` remains
//! demonstrated-ignored, so the IDRs a guest run saw at 1/31/61/91 are still this encoder's own
//! choice, not `OXPROTO.md` §9.1's two named events — forcing keyframes on a schedule this crate
//! controls, rather than asking the encoder for one, is the planned fix; and whether the protocol
//! needs a keyframe-request message at all, for a transport (unlike today's TCP) that can lose a
//! frame with no resize in sight to recover at, is a question for `docs/design/OXPROTO.md`'s
//! owner, not something to decide unilaterally here. All of this is exactly the class of bug the
//! project's own history says only shows up by running the code — see `crate::win::capture`'s
//! doc comment.

use std::collections::HashMap;
use std::ffi::c_void;

use windows::core::{Interface, Result as WinResult, GUID, VARIANT};
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Media::MediaFoundation::{
    eAVEncCommonRateControlMode_LowDelayVBR, eAVEncH264VProfile_ConstrainedBase,
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode,
    CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize, CODECAPI_AVEncMPVProfile,
    CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVEncVideoMaxNumRefFrame,
    CODECAPI_AVEncVideoTemporalLayerCount, CODECAPI_AVLowLatencyMode, ICodecAPI, IMFActivate,
    IMFMediaBuffer, IMFSample, IMFTransform, MFCreateMediaType, MFCreateMemoryBuffer,
    MFCreateSample, MFMediaType_Video, MFStartup, MFTEnumEx, MFVideoFormat_H264,
    MFVideoFormat_NV12, MFVideoInterlace_Progressive, MFSTARTUP_FULL, MFT_CATEGORY_VIDEO_ENCODER,
    MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_MPEG2_PROFILE, MF_MT_SUBTYPE, MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION,
};
use windows::Win32::System::Com::CoTaskMemFree;

use crate::encode::{EncodedFrame, FrameEncoder};
use crate::h264;
use crate::nv12::bgra_to_nv12;
use crate::serve::SourceFrame;

/// Target bitrate. Fixed for v1 rather than driven by `QualityHint` (`OXPROTO.md` §12, not yet
/// wired to the encoder) — high enough to keep screen text and UI edges legible, an order of
/// magnitude below the `RAW_BGRA` bring-up path's ~460 Mbit/s at 800×600×30fps, which is the
/// whole point of this file existing.
const TARGET_BITRATE_BPS: u32 = 6_000_000;

/// `CODECAPI_AVEncMPVGOPSize`, in frames. `OXPROTO.md` §9.1 names exactly two events that
/// produce a keyframe — the first frame of a window's session, and a resolution change — and
/// nothing else; it does not call for periodic re-keying, and this crate never asks for one
/// through `CODECAPI_AVEncVideoForceKeyFrame` outside those two cases. Left unset, an encoder
/// MFT is free to insert its own periodic sync point anyway — commonly once per second's worth
/// of configured frame rate, which is exactly what a guest run turned up: the client's decoder
/// rejected one access unit in every thirty, at a fixed period and offset, on a session encoding
/// at 30 fps. That is a GOP-boundary signature, not a random one. A value this large (at 30 fps,
/// over nine hours) is chosen over `0` specifically because this crate cannot confirm on real
/// hardware whether `0` means "no periodic key frame" or is clamped to some encoder-specific
/// default instead — an unambiguously huge value pushes the interval past anything a real
/// session reaches before a resolution change rebuilds the encoder anyway, without depending on
/// a special-cased meaning for one particular number that varies by vendor.
const GOP_SIZE_FRAMES: u32 = 1_000_000;

/// `CODECAPI_AVEncVideoMaxNumRefFrame`. A low-latency screen stream has no B-frames (no
/// reordering) and rarely enough motion complexity for a second reference frame to earn back the
/// decoder memory and prediction-search cost it adds; one is exactly what a P-frame needs and
/// nothing more. Verified from the bitstream, not trusted from the property — three `ICodecAPI`
/// properties on this same encoder have already been demonstrated accepted and silently ignored,
/// so `crate::h264::sps_ref_frame_info`'s `max_num_ref_frames` reading is the number that
/// actually matters; see `DIAGNOSTIC_FRAME_LIMIT`'s per-frame log.
const MAX_REF_FRAMES: u32 = 1;

/// How many access units, per [`WindowEncoder`], to log the raw (pre-[`h264::reframe`]) NAL
/// makeup of. Diagnostic only, requested to chase down the GOP-boundary decode failure
/// [`GOP_SIZE_FRAMES`] targets — kept bounded so a long session does not spam stderr forever once
/// the question it exists to answer has been answered.
const DIAGNOSTIC_FRAME_LIMIT: u32 = 100;

/// Which kind of H.264 encoder [`probe_h264_support`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderKind {
    /// A hardware-accelerated encoder MFT (`MFT_ENUM_FLAG_HARDWARE`).
    Hardware,
    /// Media Foundation's built-in software encoder (`MFT_ENUM_FLAG_SYNCMFT`), present on every
    /// modern Windows install regardless of GPU support.
    Software,
}

impl std::fmt::Display for EncoderKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            EncoderKind::Hardware => "hardware",
            EncoderKind::Software => "software",
        })
    }
}

/// Whether this guest can produce H.264, and which kind of encoder it would use. Calls
/// `MFStartup` as a side effect (idempotent for the life of this process — `oxagent` never
/// calls `MFShutdown`, since it never stops needing Media Foundation once this succeeds).
///
/// `None` means neither a hardware nor the built-in software H.264 encoder MFT could be
/// activated — `RAW_BGRA` remains the only codec this session can offer.
pub fn probe_h264_support() -> Option<EncoderKind> {
    // SAFETY: `MF_VERSION`/`MFSTARTUP_FULL` are well-known constants; no other preconditions.
    if unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.is_err() {
        return None;
    }
    if create_transform(EncoderKind::Hardware).is_ok() {
        return Some(EncoderKind::Hardware);
    }
    if create_transform(EncoderKind::Software).is_ok() {
        return Some(EncoderKind::Software);
    }
    None
}

/// Round a dimension up to the next even number. NV12 subsamples chroma 2:1 in each dimension,
/// so an odd coded size has no last chroma sample to pair with; edge-replicating one extra row
/// or column of the source loses nothing a lossy codec was not already going to blur anyway.
fn pad_even(x: u16) -> u16 {
    x + (x % 2)
}

/// Enumerate and activate an H.264 encoder MFT of the requested kind. Hardware is filtered and
/// sorted by `MFT_ENUM_FLAG_SORTANDFILTER` (best-first, and drops MFTs Windows itself does not
/// trust); software asks specifically for the synchronous, always-present built-in encoder
/// rather than `MFT_ENUM_FLAG_ALL`, which could just as easily hand back another, possibly
/// broken, hardware entry.
fn create_transform(kind: EncoderKind) -> WinResult<IMFTransform> {
    let flags = match kind {
        EncoderKind::Hardware => MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
        EncoderKind::Software => MFT_ENUM_FLAG_SYNCMFT,
    };
    let output_type = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };

    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count: u32 = 0;
    // SAFETY: `activates`/`count` are valid, uniquely-owned out-parameters; `output_type` lives
    // for the duration of this call.
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            None,
            Some(&output_type),
            &mut activates,
            &mut count,
        )?;
    }

    let activate =
        first_activate(activates, count).ok_or_else(|| windows::core::Error::from(E_FAIL))?;
    // SAFETY: `activate` is a valid `IMFActivate` this function just obtained from `MFTEnumEx`.
    unsafe { activate.ActivateObject::<IMFTransform>() }
}

/// Take ownership of the first entry in an `MFTEnumEx` result array, correctly releasing every
/// other entry, then free the array itself.
///
/// # Safety invariant this relies on
/// `MFTEnumEx` allocates `activates` with `CoTaskMemAlloc` as an array of `count` `IMFActivate`
/// pointers, each already holding one COM reference owned by the caller (standard COM
/// out-array convention). `Option<IMFActivate>` has the same layout as that raw pointer
/// (null-pointer-optimized), so reading each slot with `ptr::read` takes ownership of exactly
/// the reference `MFTEnumEx` handed over, without re-deriving it from memory this function does
/// not own. Every slot not returned is dropped when `owned`'s iterator is dropped, which
/// releases it properly; the raw buffer is freed separately via `CoTaskMemFree`, since Rust's
/// allocator must never be asked to deallocate memory `CoTaskMemAlloc` produced.
fn first_activate(activates: *mut Option<IMFActivate>, count: u32) -> Option<IMFActivate> {
    if activates.is_null() || count == 0 {
        return None;
    }
    let mut owned: Vec<Option<IMFActivate>> = Vec::with_capacity(count as usize);
    for i in 0..count as usize {
        // SAFETY: see the function doc — `activates.add(i)` is one of `count` valid,
        // properly-aligned, initialized slots.
        owned.push(unsafe { std::ptr::read(activates.add(i)) });
    }
    // SAFETY: the raw buffer was allocated by `MFTEnumEx` via `CoTaskMemAlloc`; every element
    // it held has already been moved out into `owned` above, so this frees only the array
    // storage itself, not anything still logically owned by a live `IMFActivate`.
    unsafe {
        CoTaskMemFree(Some(activates.cast::<c_void>().cast_const()));
    }
    owned.into_iter().flatten().next()
}

/// Read back a `u32`-valued `ICodecAPI` property, or `None` if the driver does not support
/// `GetValue` for it, or the value it returns cannot be coerced to `u32`. Used only for
/// diagnostics: "check what you are actually getting" — a driver accepting `SetValue` is not
/// proof the property took effect, since `SetValue` itself is already best-effort throughout this
/// file (see the module doc).
fn get_u32_property(api: &ICodecAPI, property: &GUID) -> Option<u32> {
    // SAFETY: `api` is a live `ICodecAPI`; `property` is always one of this file's own
    // `CODECAPI_*` constants.
    let value = unsafe { api.GetValue(property) }.ok()?;
    u32::try_from(&value).ok()
}

/// One window's live Media Foundation H.264 encoder stream.
struct WindowEncoder {
    /// Native handle, kept only for `DIAGNOSTIC_FRAME_LIMIT` logging — nothing in this struct's
    /// own logic needs it, since it is already keyed by handle in `WinFrameEncoder`.
    handle: isize,
    /// How many access units this instance has emitted from `poll` so far, counted only up to
    /// `DIAGNOSTIC_FRAME_LIMIT` — see there.
    frames_seen: u32,
    transform: IMFTransform,
    codec_api: Option<ICodecAPI>,
    /// Coded picture size this transform was configured for (after [`pad_even`]). A later
    /// resolution change rebuilds the whole `WindowEncoder` rather than reconfiguring this one
    /// live, so this never changes for the lifetime of a given instance.
    width: u32,
    height: u32,
    /// Whether the encoder allocates its own output samples
    /// (`MFT_OUTPUT_STREAM_PROVIDES_SAMPLES`) or expects the caller to. Encoder MFTs
    /// overwhelmingly expect the caller to; checked rather than assumed, per this file's own
    /// warning about trusting attribute names.
    provides_output_samples: bool,
    /// Output buffer size to allocate when `!provides_output_samples`, from
    /// `GetOutputStreamInfo`.
    output_buffer_size: u32,
    params: h264::ParamSets,
    /// Monotonic 100ns-tick sample clock. Only needs to be monotonic and roughly evenly spaced
    /// for the encoder's own internal rate control — it is not derived from, and does not need
    /// to match, the session's own `captured_us` clock.
    next_sample_time_hns: i64,
    frame_duration_hns: i64,
}

impl WindowEncoder {
    fn new(
        handle: isize,
        kind: EncoderKind,
        width: u16,
        height: u16,
        target_fps: u16,
    ) -> WinResult<Self> {
        let width = u32::from(pad_even(width)).max(2);
        let height = u32::from(pad_even(height)).max(2);
        let transform = create_transform(kind)?;

        // Hardware MFTs are commonly asynchronous; unlock synchronous driving rather than
        // implement the `IMFMediaEventGenerator` event loop — see the module doc.
        // SAFETY: `transform` is a live `IMFTransform` this function just obtained.
        let attrs = unsafe { transform.GetAttributes()? };
        // SAFETY: `attrs` is a live `IMFAttributes` this function just obtained.
        let is_async = unsafe { attrs.GetUINT32(&MF_TRANSFORM_ASYNC) }.unwrap_or(0) != 0;
        if is_async {
            // SAFETY: `attrs` is a live `IMFAttributes`; failure here is non-fatal — later calls
            // will simply fail loudly if the unlock did not take, rather than silently.
            unsafe {
                let _ = attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1);
            }
        }

        // Output type first: encoder MFTs commonly need it set before they will accept an input
        // type at all, since the input type negotiation can depend on the chosen output.
        // SAFETY: no preconditions beyond Media Foundation being started (`probe_h264_support`
        // already called `MFStartup` before any `WindowEncoder` can exist).
        let output_type = unsafe { MFCreateMediaType()? };
        // SAFETY: `output_type` was just created by this function and is fully owned here.
        unsafe {
            output_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            output_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            output_type.SetUINT64(&MF_MT_FRAME_SIZE, pack_u64(width, height))?;
            output_type.SetUINT64(&MF_MT_FRAME_RATE, pack_u64(u32::from(target_fps.max(1)), 1))?;
            output_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            output_type.SetUINT32(&MF_MT_AVG_BITRATE, TARGET_BITRATE_BPS)?;
            // Constrained Baseline, on the *media type* rather than only through `ICodecAPI`
            // (`CODECAPI_AVEncMPVProfile`, still set best-effort below). A guest run proved two
            // other `ICodecAPI` properties on this exact encoder are accepted by `SetValue` and
            // then ignored — including the no-B-frames one, which is how the client ended up
            // decoding actual B-pictures under a stream this crate believed was violating
            // nothing. A media-type attribute is not the same kind of promise: `SetOutputType`
            // negotiates it, so the transform either accepts Constrained Baseline here (this `?`
            // succeeds) or this function fails loudly instead of silently building an encoder
            // that emits Main or High anyway. `MF_MT_MPEG2_PROFILE` is Media Foundation's actual
            // attribute for signalling H.264 profile on the output type — the "MPEG2" in the name
            // is legacy, not a mistake; there is no separate H.264-named attribute for this.
            output_type.SetUINT32(
                &MF_MT_MPEG2_PROFILE,
                eAVEncH264VProfile_ConstrainedBase.0 as u32,
            )?;
        }
        // SAFETY: `transform` is a live `IMFTransform`; `output_type` was just fully configured
        // above.
        unsafe { transform.SetOutputType(0, &output_type, 0)? };

        // Input type: discovered from what the transform itself advertises as acceptable for
        // NV12, then customized with this window's actual size/rate, rather than constructed
        // from scratch — an encoder MFT's input type can carry required attributes this file
        // has no way to know about in advance.
        let input_type = find_nv12_input_type(&transform)?;
        // SAFETY: `input_type` came from the transform's own `GetInputAvailableType`.
        unsafe {
            input_type.SetUINT64(&MF_MT_FRAME_SIZE, pack_u64(width, height))?;
            input_type.SetUINT64(&MF_MT_FRAME_RATE, pack_u64(u32::from(target_fps.max(1)), 1))?;
        }
        // SAFETY: `transform` is a live `IMFTransform`; `input_type` was just configured above.
        unsafe { transform.SetInputType(0, &input_type, 0)? };

        // `ICodecAPI` is optional: not every encoder MFT implements it, and the properties set
        // through it are best-effort even when it does — see the module doc's caveat about
        // silently-ignored properties.
        let codec_api: Option<ICodecAPI> = transform.cast().ok();
        if let Some(api) = &codec_api {
            // SAFETY: `api` is a live `ICodecAPI` on `transform`; every `SetValue` here is
            // best-effort (`let _ =`) since a driver may legitimately not support a given
            // property.
            unsafe {
                let _ = api.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true));
                let _ = api.SetValue(
                    &CODECAPI_AVEncCommonRateControlMode,
                    &VARIANT::from(eAVEncCommonRateControlMode_LowDelayVBR.0 as u32),
                );
                let _ = api.SetValue(
                    &CODECAPI_AVEncCommonMeanBitRate,
                    &VARIANT::from(TARGET_BITRATE_BPS),
                );
                // Mandatory, not best-effort in spirit even though the call itself is
                // best-effort: OXPROTO.md §9.1 requires zero B-frames so capture order, encode
                // order and `frame_id` order stay identical — §12's flow control (drop the
                // oldest unacknowledged frame) is unsound the moment a later picture can
                // reference one that was skipped.
                let _ = api.SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &VARIANT::from(0u32));
                // See `GOP_SIZE_FRAMES`: this crate is the only thing that should ever decide
                // when a new keyframe starts.
                let _ = api.SetValue(&CODECAPI_AVEncMPVGOPSize, &VARIANT::from(GOP_SIZE_FRAMES));
                // Constrained Baseline, explicitly: left unset, Media Foundation is free to
                // pick its own profile (commonly Main or High), and a real guest capture found
                // every rejected access unit was a disposable (`nal_ref_idc = 0`) non-IDR slice
                // on a strict period, matching temporal-layer/hierarchical-P structuring this
                // crate never asked for and had never explicitly turned off. Setting the profile
                // does not by itself guarantee that structuring stops — Constrained Baseline
                // still permits non-reference P-slices — so `CODECAPI_AVEncVideoTemporalLayerCount`
                // is pinned to 0 right below it for the same reason, not as a fallback.
                let _ = api.SetValue(
                    &CODECAPI_AVEncMPVProfile,
                    &VARIANT::from(eAVEncH264VProfile_ConstrainedBase.0 as u32),
                );
                let _ = api.SetValue(&CODECAPI_AVEncVideoTemporalLayerCount, &VARIANT::from(0u32));
                // See `MAX_REF_FRAMES`.
                let _ = api.SetValue(
                    &CODECAPI_AVEncVideoMaxNumRefFrame,
                    &VARIANT::from(MAX_REF_FRAMES),
                );
            }

            // "Check what you are actually getting rather than what you set": read the
            // properties above straight back, immediately, rather than trusting that `SetValue`
            // succeeding means the driver actually adopted them. Best-effort like everything
            // else here — `None` just means this driver does not support reading a property back,
            // not that the `SetValue` above failed — and, per `MAX_REF_FRAMES`'s doc, not
            // authoritative even when it is `Some` and matches: see the per-frame SPS log below
            // for what the bitstream itself ends up saying.
            eprintln!(
                "oxagent: h264: window={handle:#x} encoder configured: profile requested={} reported={:?}, temporal_layers requested=0 reported={:?}, max_ref_frames requested={MAX_REF_FRAMES} reported={:?}",
                eAVEncH264VProfile_ConstrainedBase.0,
                get_u32_property(api, &CODECAPI_AVEncMPVProfile),
                get_u32_property(api, &CODECAPI_AVEncVideoTemporalLayerCount),
                get_u32_property(api, &CODECAPI_AVEncVideoMaxNumRefFrame),
            );
        }

        let output_info = unsafe { transform.GetOutputStreamInfo(0)? };
        let provides_output_samples =
            output_info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;

        // SAFETY: `transform` is fully configured (both media types and, best-effort, codec
        // properties) at this point, which is the documented precondition for these messages.
        unsafe {
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }

        let frame_duration_hns = 10_000_000i64 / i64::from(target_fps.max(1));

        Ok(Self {
            handle,
            frames_seen: 0,
            transform,
            codec_api,
            width,
            height,
            provides_output_samples,
            output_buffer_size: output_info.cbSize,
            params: h264::ParamSets::default(),
            next_sample_time_hns: 0,
            frame_duration_hns,
        })
    }

    /// Whether this instance is still usable for `frame` — false after a coded-size change,
    /// which the caller must handle by building a fresh `WindowEncoder`.
    fn matches(&self, frame: &SourceFrame) -> bool {
        u32::from(pad_even(frame.width)) == self.width
            && u32::from(pad_even(frame.height)) == self.height
    }

    fn submit(&mut self, frame: &SourceFrame, force_keyframe: bool) {
        if force_keyframe {
            if let Some(api) = &self.codec_api {
                // SAFETY: `api` is a live `ICodecAPI` on `self.transform`.
                unsafe {
                    let _ = api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(true));
                }
            }
        }

        let Ok(sample) = self.build_input_sample(frame) else {
            return;
        };
        // SAFETY: `sample` is a fully populated `IMFSample` built for this transform's negotiated
        // NV12 input type.
        let result = unsafe { self.transform.ProcessInput(0, &sample, 0) };
        // An encoder that cannot take more input right now (still working on a previous frame)
        // simply does not get this one — the same "newest content wins over queueing"
        // philosophy `crate::pacing::FrameBudget` already applies one stage later, applied here
        // to the encoder's own internal pipeline instead of the network.
        let _ = result;
    }

    fn build_input_sample(&mut self, frame: &SourceFrame) -> WinResult<IMFSample> {
        // The captured frame may be smaller than `self.width`/`self.height` by exactly the
        // padding `pad_even` added; `bgra_to_nv12` needs an even-sized buffer, so pad here by
        // reusing the last valid row/column — edge-replication, not resampling.
        let padded = pad_bgra(
            &frame.data,
            frame.width,
            frame.height,
            self.width,
            self.height,
        );
        let nv12 = bgra_to_nv12(&padded, self.width as usize, self.height as usize);

        let buffer = unsafe { MFCreateMemoryBuffer(nv12.len() as u32)? };
        {
            let mut ptr = std::ptr::null_mut();
            // SAFETY: `buffer` was just created with exactly `nv12.len()` bytes of capacity;
            // unlocked on every path via the guard below.
            unsafe { buffer.Lock(&mut ptr, None, None)? };
            let guard = LockGuard(&buffer);
            // SAFETY: `ptr` is valid for `nv12.len()` writable bytes per the successful `Lock`
            // above.
            unsafe {
                std::ptr::copy_nonoverlapping(nv12.as_ptr(), ptr, nv12.len());
            }
            drop(guard);
            unsafe { buffer.SetCurrentLength(nv12.len() as u32)? };
        }

        let sample = unsafe { MFCreateSample()? };
        unsafe {
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(self.next_sample_time_hns)?;
            sample.SetSampleDuration(self.frame_duration_hns)?;
        }
        self.next_sample_time_hns += self.frame_duration_hns;
        Ok(sample)
    }

    fn poll(&mut self) -> Option<EncodedFrame> {
        // If the transform does not allocate its own output samples (the common case for an
        // encoder MFT), the caller must hand it a sample with an attached buffer sized from
        // `GetOutputStreamInfo`; if it does, `pSample` must go in empty and comes back filled.
        let sample = if self.provides_output_samples {
            None
        } else {
            // SAFETY: sizes and wires together a fresh, empty output sample; nothing here reads
            // or writes buffer contents.
            let buffer = unsafe { MFCreateMemoryBuffer(self.output_buffer_size) }.ok()?;
            let s = unsafe { MFCreateSample() }.ok()?;
            unsafe { s.AddBuffer(&buffer) }.ok()?;
            Some(s)
        };

        let mut output = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: std::mem::ManuallyDrop::new(sample),
            dwStatus: 0,
            pEvents: std::mem::ManuallyDrop::new(None),
        };
        let mut status: u32 = 0;
        // SAFETY: `output` is a single, fully-initialized `MFT_OUTPUT_DATA_BUFFER`; its sample
        // (if any) was just allocated above with the size `GetOutputStreamInfo` reported.
        let result = unsafe {
            self.transform
                .ProcessOutput(0, std::slice::from_mut(&mut output), &mut status)
        };

        let produced = match result {
            Ok(()) => std::mem::ManuallyDrop::into_inner(output.pSample),
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => None,
            Err(_) => None,
        };
        // The event collection, if the transform populated one, is not consumed here; drop it
        // explicitly rather than leaking the `IMFCollection` reference.
        drop(std::mem::ManuallyDrop::into_inner(output.pEvents));

        let sample = produced?;
        let buffer = unsafe { sample.ConvertToContiguousBuffer() }.ok()?;
        let raw = read_buffer(&buffer)?;

        if self.frames_seen < DIAGNOSTIC_FRAME_LIMIT {
            self.frames_seen += 1;
            // Logged from `raw`, before `reframe` below reorders or strips anything — this is
            // what the transform itself actually produced. See `DIAGNOSTIC_FRAME_LIMIT`.
            // `nal_ref_idc` is included per-NAL specifically to catch disposable, non-reference
            // pictures (`ref_idc == 0`) — a real guest capture found every periodically-rejected
            // access unit was exactly that.
            let nals: Vec<String> = h264::nal_summary(&raw)
                .into_iter()
                .map(|(kind, ref_idc, len)| {
                    format!("{}:ref_idc={ref_idc}:{len}", h264::nal_type_name(kind))
                })
                .collect();
            let profile = h264::first_sps_profile(&raw)
                .map(|(p, c, l)| {
                    format!(" sps_profile_idc={p} sps_constraint_flags={c:#010b} sps_level_idc={l}")
                })
                .unwrap_or_default();
            // The authoritative check for `MAX_REF_FRAMES`: read straight from the bitstream's
            // own SPS, not from `ICodecAPI::GetValue`, which has already been shown to echo back
            // values this encoder does not actually honour.
            let ref_frames = h264::first_sps_ref_frame_info(&raw)
                .map(|info| {
                    format!(
                        " sps_max_num_ref_frames={} sps_pic_order_cnt_type={}",
                        info.max_num_ref_frames, info.pic_order_cnt_type
                    )
                })
                .unwrap_or_default();
            eprintln!(
                "oxagent: h264: window={:#x} frame={} nals=[{}]{profile}{ref_frames}",
                self.handle,
                self.frames_seen,
                nals.join(", ")
            );
        }

        let (data, is_idr) = h264::reframe(&raw, &mut self.params);
        if data.is_empty() {
            return None;
        }
        Some(EncodedFrame {
            data,
            keyframe: is_idr,
            width: self.width.min(u32::from(u16::MAX)) as u16,
            height: self.height.min(u32::from(u16::MAX)) as u16,
        })
    }
}

/// Copy an `IMFMediaBuffer`'s current contents into a plain `Vec<u8>`.
fn read_buffer(buffer: &IMFMediaBuffer) -> Option<Vec<u8>> {
    let mut ptr = std::ptr::null_mut();
    let mut len: u32 = 0;
    // SAFETY: `buffer` is a valid, locked-for-reading `IMFMediaBuffer`; unlocked on every path
    // via the guard below.
    unsafe { buffer.Lock(&mut ptr, None, Some(&mut len)).ok()? };
    let guard = LockGuard(buffer);
    // SAFETY: `ptr` is valid for `len` readable bytes per the successful `Lock` above.
    let data = unsafe { std::slice::from_raw_parts(ptr, len as usize) }.to_vec();
    drop(guard);
    Some(data)
}

/// Unlocks an `IMFMediaBuffer` on drop, so every return path — including an early `?` —
/// releases the lock instead of leaving it held until the buffer itself is dropped.
struct LockGuard<'a>(&'a IMFMediaBuffer);
impl Drop for LockGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: `self.0` was locked by whoever constructed this guard, immediately before
        // constructing it, and is not unlocked anywhere else.
        unsafe {
            let _ = self.0.Unlock();
        }
    }
}

/// Find the transform's own advertised NV12 input type by iterating `GetInputAvailableType`,
/// rather than constructing one from scratch — an encoder can require attributes on the input
/// type this file has no way to anticipate, so starting from what the transform itself offers
/// is the more robust choice for input, in contrast to the output type (§9.1's own encode
/// parameters), which this file does construct explicitly.
fn find_nv12_input_type(
    transform: &IMFTransform,
) -> WinResult<windows::Win32::Media::MediaFoundation::IMFMediaType> {
    for index in 0.. {
        // SAFETY: `transform` is a live `IMFTransform`.
        let candidate = match unsafe { transform.GetInputAvailableType(0, index) } {
            Ok(t) => t,
            Err(_) => break,
        };
        // SAFETY: `candidate` is a live `IMFMediaType` just obtained above.
        if let Ok(subtype) = unsafe { candidate.GetGUID(&MF_MT_SUBTYPE) } {
            if subtype == MFVideoFormat_NV12 {
                return Ok(candidate);
            }
        }
    }
    Err(windows::core::Error::from(E_FAIL))
}

/// Pack two `u32`s into the `u64` `MF_MT_FRAME_SIZE`/`MF_MT_FRAME_RATE` (and similar) attributes
/// use: high 32 bits first, low 32 bits second. This is `MFSetAttributeSize`/
/// `MFSetAttributeRatio`'s packing — inline C++ helpers in `mfapi.h`, not exported symbols
/// windows-rs binds, so this crate does the packing itself via the underlying `SetUINT64`.
fn pack_u64(high: u32, low: u32) -> u64 {
    (u64::from(high) << 32) | u64::from(low)
}

/// Pad a tightly-packed top-down BGRA8 image from `(src_w, src_h)` up to `(dst_w, dst_h)` by
/// edge-replicating the last valid row/column. A no-op copy when the sizes already match, which
/// is the common case — only an odd capture dimension ever needs this.
fn pad_bgra(src: &[u8], src_w: u16, src_h: u16, dst_w: u32, dst_h: u32) -> Vec<u8> {
    let (src_w, src_h) = (src_w as usize, src_h as usize);
    let (dst_w, dst_h) = (dst_w as usize, dst_h as usize);
    if src_w == dst_w && src_h == dst_h {
        return src.to_vec();
    }
    let mut out = vec![0u8; dst_w * dst_h * 4];
    for y in 0..dst_h {
        let sy = y.min(src_h.saturating_sub(1));
        for x in 0..dst_w {
            let sx = x.min(src_w.saturating_sub(1));
            let src_i = (sy * src_w + sx) * 4;
            let dst_i = (y * dst_w + x) * 4;
            if src_i + 4 <= src.len() {
                out[dst_i..dst_i + 4].copy_from_slice(&src[src_i..src_i + 4]);
            }
        }
    }
    out
}

/// The Windows implementation of [`FrameEncoder`], holding one live [`WindowEncoder`] per
/// window, created lazily on first submission and rebuilt whenever the coded size changes.
pub struct WinFrameEncoder {
    kind: EncoderKind,
    target_fps: u16,
    encoders: HashMap<isize, WindowEncoder>,
    /// Handles whose `WindowEncoder` construction failed, and the size (as reported by
    /// `SourceFrame`, before `pad_even`) it failed at. Consulted by `failed` and by `submit`
    /// itself, so a size that has already failed once is not retried every single tick forever —
    /// only a *different* size (a resolution change) gets a fresh attempt, since a different
    /// size might not hit whatever the original failure was.
    failed: HashMap<isize, (u16, u16)>,
}

impl WinFrameEncoder {
    /// `kind` should be whatever [`probe_h264_support`] returned — this does not re-probe.
    pub fn new(kind: EncoderKind, target_fps: u16) -> Self {
        Self {
            kind,
            target_fps,
            encoders: HashMap::new(),
            failed: HashMap::new(),
        }
    }
}

impl FrameEncoder for WinFrameEncoder {
    fn submit(&mut self, handle: isize, frame: &SourceFrame, force_keyframe: bool) {
        if self.failed.get(&handle) == Some(&(frame.width, frame.height)) {
            // Already known broken at exactly this size — see `failed`'s doc. The caller checks
            // `Self::failed` after this returns and falls back accordingly; nothing to do here.
            return;
        }
        let needs_rebuild = match self.encoders.get(&handle) {
            Some(enc) => !enc.matches(frame),
            None => true,
        };
        if needs_rebuild {
            match WindowEncoder::new(
                handle,
                self.kind,
                frame.width,
                frame.height,
                self.target_fps,
            ) {
                Ok(enc) => {
                    self.encoders.insert(handle, enc);
                    self.failed.remove(&handle);
                }
                Err(err) => {
                    eprintln!(
                        "oxagent: h264: encoder setup failed for {handle:#x} at {}x{}: {err} — \
                         falling back to RAW_BGRA for this window until its resolution changes",
                        frame.width, frame.height
                    );
                    self.encoders.remove(&handle);
                    self.failed.insert(handle, (frame.width, frame.height));
                    return;
                }
            }
        }
        // A window whose encoder was just (re)built has nothing yet to reference, so this
        // submission must be a keyframe regardless of what the caller asked for.
        let force_keyframe = force_keyframe || needs_rebuild;
        if let Some(enc) = self.encoders.get_mut(&handle) {
            enc.submit(frame, force_keyframe);
        }
    }

    fn poll(&mut self, handle: isize) -> Option<EncodedFrame> {
        self.encoders.get_mut(&handle)?.poll()
    }

    fn forget(&mut self, handle: isize) {
        self.encoders.remove(&handle);
        self.failed.remove(&handle);
    }

    fn failed(&self, handle: isize) -> bool {
        self.failed.contains_key(&handle)
    }
}
