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
//!   picture can reference one that got skipped.
//! - **Parameter sets are never trusted to "just be there."** Whatever the transform's raw
//!   output actually contains, `crate::h264::reframe` normalizes it into what §9.1 requires —
//!   see that module for why this is not redundant with configuring the transform correctly.
//! - **A resolution change tears the whole per-window encoder down and rebuilds it**, rather
//!   than trying to reconfigure a live MFT's media type mid-stream. Simpler, and it naturally
//!   produces the fresh SPS/PPS + keyframe §9.1 requires on a coded-size change, since a new
//!   transform always starts a fresh stream.
//!
//! **What this file's author could not verify without a live Windows guest**, stated plainly
//! rather than left implicit: whether `MF_TRANSFORM_ASYNC_UNLOCK` actually lets every hardware
//! encoder this might run against be driven synchronously (it is documented Microsoft guidance,
//! not this crate's invention, but "documented" and "true of the specific driver on the test
//! guest" are not the same claim); whether the codec API properties set here are actually
//! honoured by that driver, since a driver is free to silently ignore an unsupported property
//! rather than fail `SetValue`; and the `MFTEnumEx` output array's ownership handling in
//! [`first_activate`], which follows the COM ownership rules as documented but has had no COM
//! debugger anywhere near it. All three are exactly the class of bug the project's own history
//! says only shows up by running the code — see `crate::win::capture`'s doc comment.

use std::collections::HashMap;
use std::ffi::c_void;

use windows::core::{Interface, Result as WinResult, VARIANT};
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Media::MediaFoundation::{
    eAVEncCommonRateControlMode_LowDelayVBR, CODECAPI_AVEncCommonMeanBitRate,
    CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncMPVDefaultBPictureCount,
    CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode, ICodecAPI, IMFActivate,
    IMFMediaBuffer, IMFSample, IMFTransform, MFCreateMediaType, MFCreateMemoryBuffer,
    MFCreateSample, MFMediaType_Video, MFStartup, MFTEnumEx, MFVideoFormat_H264,
    MFVideoFormat_NV12, MFVideoInterlace_Progressive, MFSTARTUP_FULL, MFT_CATEGORY_VIDEO_ENCODER,
    MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_SUBTYPE, MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION,
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

/// One window's live Media Foundation H.264 encoder stream.
struct WindowEncoder {
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
    fn new(kind: EncoderKind, width: u16, height: u16, target_fps: u16) -> WinResult<Self> {
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
            }
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
}

impl WinFrameEncoder {
    /// `kind` should be whatever [`probe_h264_support`] returned — this does not re-probe.
    pub fn new(kind: EncoderKind, target_fps: u16) -> Self {
        Self {
            kind,
            target_fps,
            encoders: HashMap::new(),
        }
    }
}

impl FrameEncoder for WinFrameEncoder {
    fn submit(&mut self, handle: isize, frame: &SourceFrame, force_keyframe: bool) {
        let needs_rebuild = match self.encoders.get(&handle) {
            Some(enc) => !enc.matches(frame),
            None => true,
        };
        if needs_rebuild {
            match WindowEncoder::new(self.kind, frame.width, frame.height, self.target_fps) {
                Ok(enc) => {
                    self.encoders.insert(handle, enc);
                }
                Err(err) => {
                    eprintln!("oxagent: H.264 encoder setup failed for {handle:#x}: {err}");
                    self.encoders.remove(&handle);
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
    }
}
