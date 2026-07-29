//! Annex-B H.264 NAL utilities: splitting, and reframing an encoder's raw output into exactly
//! what `docs/design/OXPROTO.md` §9.1 requires `FrameData.data` to contain.
//!
//! Deliberately platform-independent, unlike the encoder that calls it
//! (`crate::win::encode::WinFrameEncoder`): the wire-format rules in §9.1 (NAL ordering,
//! parameter sets in-band on every keyframe, `KEYFRAME` meaning IDR) are pure byte-level
//! bookkeeping with no Windows dependency, and getting them wrong is a protocol bug, not a
//! Media Foundation bug — worth its own test coverage that runs on the Linux build host,
//! completely independent of whether the encoder itself is ever exercised there.
//!
//! Why this module exists at all instead of trusting the encoder to already emit spec-compliant
//! output: Media Foundation's H.264 encoders are not guaranteed to repeat SPS/PPS on every IDR
//! by default — behavior here varies by vendor and driver, which is exactly the kind of thing
//! "verify what it actually emits rather than trusting the attribute name" warns about. Rather
//! than depend on a specific encoder attribute working as documented, [`reframe`] inspects the
//! encoder's actual output NAL-by-NAL, caches the most recent parameter sets it has seen, and
//! injects them itself whenever a keyframe's raw output does not already carry them. The
//! invariant this file guarantees holds regardless of what any particular encoder does.

/// `nal_unit_type` values this module cares about (ITU-T H.264 §7.4.1).
pub mod nal_type {
    /// Coded slice of a non-IDR picture.
    pub const SLICE: u8 = 1;
    /// Supplemental enhancement information.
    pub const SEI: u8 = 6;
    /// Sequence parameter set.
    pub const SPS: u8 = 7;
    /// Picture parameter set.
    pub const PPS: u8 = 8;
    /// Access unit delimiter.
    pub const AUD: u8 = 9;
    /// Coded slice of an IDR picture.
    pub const SLICE_IDR: u8 = 5;
}

/// One NAL unit as found in an Annex-B byte stream: `kind` is `nal_unit_type` (the low 5 bits
/// of the header byte); `payload` is the complete NAL — header byte included — with the start
/// code and any trailing zero padding excluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Nal<'a> {
    kind: u8,
    payload: &'a [u8],
}

/// Split an Annex-B byte stream into its NAL units. Accepts both the 3-byte (`00 00 01`) and
/// 4-byte (`00 00 00 01`) start code forms per unit, matching what §9.1 requires a decoder to
/// tolerate even though this crate always *emits* the 4-byte form.
///
/// A byte stream with no start code at all yields no NALs (not an error): callers treat "raw
/// encoder output we could not parse" as "nothing to reframe" rather than panicking on it.
fn split_annexb(data: &[u8]) -> Vec<Nal<'_>> {
    // Positions where a start code begins, together with how many bytes it occupies.
    let mut starts: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 {
            if data[i + 2] == 1 {
                starts.push((i, 3));
                i += 3;
                continue;
            }
            if i + 4 <= data.len() && data[i + 2] == 0 && data[i + 3] == 1 {
                starts.push((i, 4));
                i += 4;
                continue;
            }
        }
        i += 1;
    }

    let mut nals = Vec::with_capacity(starts.len());
    for (idx, &(pos, code_len)) in starts.iter().enumerate() {
        let payload_start = pos + code_len;
        let payload_end = starts.get(idx + 1).map_or(data.len(), |&(next, _)| next);
        if payload_start >= payload_end {
            continue; // an empty or truncated NAL (e.g. a dangling start code); skip it.
        }
        let payload = &data[payload_start..payload_end];
        // The NAL header is one byte; bits 3-7 (0-indexed from the MSB, i.e. the low 5 bits of
        // the byte) are nal_unit_type.
        let kind = payload[0] & 0x1F;
        nals.push(Nal { kind, payload });
    }
    nals
}

/// Cached parameter sets: the most recent SPS and PPS NAL units seen (header byte included,
/// start code excluded), used to backfill a keyframe whose raw encoder output did not carry
/// them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParamSets {
    sps: Vec<u8>,
    pps: Vec<u8>,
}

impl ParamSets {
    /// Whether both an SPS and a PPS have been captured. Used by the Windows encoder
    /// (`crate::win::encode`) to decide whether it is safe to hand a keyframe to the client
    /// yet; not exercised on the host build, where nothing ever populates a `ParamSets`.
    #[allow(dead_code)]
    pub(crate) fn is_complete(&self) -> bool {
        !self.sps.is_empty() && !self.pps.is_empty()
    }
}

const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// Append one NAL as Annex-B: a 4-byte start code followed by `payload` verbatim.
fn push_nal(out: &mut Vec<u8>, payload: &[u8]) {
    out.extend_from_slice(&START_CODE);
    out.extend_from_slice(payload);
}

/// Turn one encoder-produced access unit into the exact byte layout `OXPROTO.md` §9.1 requires,
/// normalizing whatever the encoder actually emitted (whether or not it repeated SPS/PPS,
/// whether it used 3- or 4-byte start codes, whatever order it put things in) into the spec's
/// required order: at most one AUD, then any SEI, then — on a keyframe only — SPS and PPS, then
/// the slice NAL(s), then anything else.
///
/// `cached` is updated in place whenever this access unit carries its own SPS/PPS, so a later
/// keyframe that doesn't repeat them can still be backfilled from here. Returns the reframed
/// bytes and whether this access unit is a keyframe (contains an IDR slice) — the caller uses
/// the latter to set `frame_flag::KEYFRAME` (`OXPROTO.md` §9.1: KEYFRAME means IDR specifically,
/// never "any I-frame").
pub fn reframe(raw: &[u8], cached: &mut ParamSets) -> (Vec<u8>, bool) {
    let nals = split_annexb(raw);

    let mut aud: Option<&[u8]> = None;
    let mut sei: Vec<&[u8]> = Vec::new();
    let mut sps: Option<&[u8]> = None;
    let mut pps: Option<&[u8]> = None;
    let mut slices: Vec<&[u8]> = Vec::new();
    let mut trailing: Vec<&[u8]> = Vec::new();
    let mut is_idr = false;

    for nal in &nals {
        match nal.kind {
            nal_type::AUD if aud.is_none() => aud = Some(nal.payload),
            nal_type::SEI => sei.push(nal.payload),
            nal_type::SPS => sps = Some(nal.payload),
            nal_type::PPS => pps = Some(nal.payload),
            nal_type::SLICE_IDR => {
                is_idr = true;
                slices.push(nal.payload);
            }
            nal_type::SLICE => slices.push(nal.payload),
            // A second AUD, or anything this table does not name, rides along after the slice
            // NAL(s) rather than being dropped — §9.1 rule 5, and design rule 6 (unknown things
            // are skipped, not treated as an error, but "skipped" here means "not reordered
            // into a slot it does not belong in", not "discarded").
            _ => trailing.push(nal.payload),
        }
    }

    // Refresh the cache whenever this access unit brought its own parameter sets — always the
    // freshest available, and this is also how a resolution change's new SPS/PPS (OXPROTO.md
    // §9.1 "Resolution changes") gets picked up for every keyframe after it.
    if let Some(s) = sps {
        cached.sps = s.to_vec();
    }
    if let Some(p) = pps {
        cached.pps = p.to_vec();
    }

    let mut out = Vec::with_capacity(raw.len() + 32);
    if let Some(a) = aud {
        push_nal(&mut out, a);
    }
    for s in &sei {
        push_nal(&mut out, s);
    }
    if is_idr {
        // Prefer what this access unit brought itself; fall back to the cache so a keyframe
        // that omitted them (an encoder that only sends parameter sets once) still gets them —
        // the entire reason this module exists. If neither is available (no encoder has ever
        // produced any and this is not itself carrying them — should not happen for a real
        // encoder's first IDR, but "should not happen" is exactly what this file exists not to
        // trust), the access unit goes out without them; a decoder attached from a truly cold
        // start would then fail to decode it, but that failure is confined to this one corrupt
        // stream rather than fabricated out of nothing.
        if let Some(s) = sps.or(if cached.sps.is_empty() {
            None
        } else {
            Some(cached.sps.as_slice())
        }) {
            push_nal(&mut out, s);
        }
        if let Some(p) = pps.or(if cached.pps.is_empty() {
            None
        } else {
            Some(cached.pps.as_slice())
        }) {
            push_nal(&mut out, p);
        }
    }
    for s in &slices {
        push_nal(&mut out, s);
    }
    for t in &trailing {
        push_nal(&mut out, t);
    }

    (out, is_idr)
}

/// `(nal_unit_type, nal_ref_idc, payload length)` — header byte included in the length — for
/// every NAL unit in `data`, in stream order: a diagnostic view of an access unit's actual,
/// as-received structure, independent of whatever [`reframe`] goes on to do with it. Exists so a
/// caller (`crate::win` encoder wrapper) can log exactly what came out of the encoder before this
/// module's own reordering, stripping, and backfilling touch it — which is the only way to see
/// something like a stray parameter set on a non-keyframe, since [`reframe`] itself would already
/// have removed it from anything logged after the fact.
///
/// `nal_ref_idc` (the header byte's bits 6-5, a 2-bit value 0-3) is included specifically because
/// it is what distinguishes a disposable, non-reference picture (`nal_ref_idc == 0`, legal for
/// any slice type including a plain P-slice, not just B-slices) from one later pictures may
/// depend on — the field a temporal-layer or hierarchical-P encoder configuration marks
/// differently on the frames it treats as expendable.
pub fn nal_summary(data: &[u8]) -> Vec<(u8, u8, usize)> {
    split_annexb(data)
        .iter()
        .map(|nal| {
            let nal_ref_idc = (nal.payload[0] >> 5) & 0x3;
            (nal.kind, nal_ref_idc, nal.payload.len())
        })
        .collect()
}

/// A short human-readable name for a `nal_unit_type`, for diagnostics only — never used anywhere
/// [`reframe`] itself makes a decision, which switches on the numeric `kind` exclusively.
pub fn nal_type_name(kind: u8) -> &'static str {
    match kind {
        nal_type::SLICE => "SLICE",
        nal_type::SLICE_IDR => "SLICE_IDR",
        nal_type::SEI => "SEI",
        nal_type::SPS => "SPS",
        nal_type::PPS => "PPS",
        nal_type::AUD => "AUD",
        _ => "OTHER",
    }
}

/// `(profile_idc, constraint_flags, level_idc)` from an SPS NAL's payload — ITU-T H.264
/// §7.3.2.1.1: three fixed, byte-aligned fields immediately after the one-byte NAL header, before
/// anything exp-golomb-coded starts, so no bitstream parser is needed to read them. `None` if
/// `payload` is too short to hold all three (a truncated or otherwise malformed SPS, treated as
/// "nothing to report" rather than panicking on it, same as the rest of this module).
///
/// `constraint_flags` packs `constraint_set0_flag` through `constraint_set5_flag` in bits 7
/// down to 2, with the low 2 bits reserved; bit 6 (`0x40`, `constraint_set1_flag`) is what turns
/// `profile_idc == 66` specifically into Constrained Baseline rather than plain Baseline. Exists
/// so a caller can log what an encoder's SPS actually says instead of what a `CODECAPI_*`
/// property was merely asked to produce — a driver accepting `ICodecAPI::SetValue` is not proof
/// it took effect.
pub fn sps_profile(payload: &[u8]) -> Option<(u8, u8, u8)> {
    let profile_idc = *payload.get(1)?;
    let constraint_flags = *payload.get(2)?;
    let level_idc = *payload.get(3)?;
    Some((profile_idc, constraint_flags, level_idc))
}

/// [`sps_profile`] for the first SPS NAL found in a raw Annex-B access unit, if any. Convenience
/// for a caller that has the whole access unit's bytes (an encoder's raw `ProcessOutput` sample,
/// say) and wants "what profile does this AU's SPS claim" without splitting it into NALs itself.
pub fn first_sps_profile(raw: &[u8]) -> Option<(u8, u8, u8)> {
    split_annexb(raw)
        .into_iter()
        .find(|nal| nal.kind == nal_type::SPS)
        .and_then(|nal| sps_profile(nal.payload))
}

/// Remove H.264 emulation-prevention bytes from a NAL payload, turning the escaped byte sequence
/// a NAL unit carries on the wire into the raw RBSP bit sequence Exp-Golomb fields are coded
/// against (ITU-T H.264 §7.4.1.1). A `0x03` byte immediately following any `00 00` two-byte run
/// inside a NAL is not data — it exists only so that run can never be mistaken for a start code —
/// and must be dropped before any bit past the fixed byte-aligned header fields can be trusted.
/// [`sps_profile`] never needed this: it only reads fixed-position bytes that come before any
/// point an emulation-prevention byte could occur this early. Anything using [`BitReader`] does.
fn unescape_rbsp(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len());
    let mut zero_run = 0u32;
    for &byte in payload {
        if zero_run >= 2 && byte == 0x03 {
            // The emulation-prevention byte itself: dropped, not copied. The run that triggered
            // it is now behind an explicit non-zero marker in the real bitstream, so it cannot
            // combine with whatever follows to form another `00 00 0x` — reset rather than
            // decrement.
            zero_run = 0;
            continue;
        }
        out.push(byte);
        zero_run = if byte == 0 { zero_run + 1 } else { 0 };
    }
    out
}

/// A read-only, MSB-first bit cursor over an already-unescaped RBSP — the bit order ITU-T
/// H.264's bitstream syntax (§7.3.2.1.1 and others) is defined in. Exposes exactly the
/// primitives that syntax needs: a fixed-width unsigned field (`u(n)`) and the two Exp-Golomb
/// codes (`ue(v)`, `se(v)`, ITU-T H.264 §9.1).
///
/// Every method returns `None` on running out of bits, the same "cannot make sense of this,
/// nothing to report" convention [`split_annexb`] and [`sps_profile`] already use for malformed
/// input, rather than panicking on a real encoder's output this crate does not otherwise control.
struct BitReader<'a> {
    data: &'a [u8],
    /// 0-indexed from the start of `data`, counting individual bits MSB-first within each byte.
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit_pos: 0 }
    }

    fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.bit_pos / 8)?;
        let shift = 7 - (self.bit_pos % 8);
        self.bit_pos += 1;
        Some(u32::from((byte >> shift) & 1))
    }

    /// `u(n)`: an unsigned fixed-width field, MSB first, `n <= 32`.
    fn u(&mut self, n: u32) -> Option<u32> {
        let mut value = 0u32;
        for _ in 0..n {
            value = (value << 1) | self.bit()?;
        }
        Some(value)
    }

    /// `ue(v)`: Exp-Golomb-coded unsigned integer. `None` if the leading-zero-bit run alone
    /// would not fit this reader's `u32` result — not a value any field this module reads is
    /// ever legally allowed to reach, so treated as malformed rather than trusted.
    fn ue(&mut self) -> Option<u32> {
        let mut leading_zeros = 0u32;
        while self.bit()? == 0 {
            leading_zeros += 1;
            if leading_zeros > 31 {
                return None;
            }
        }
        if leading_zeros == 0 {
            return Some(0);
        }
        let suffix = self.u(leading_zeros)?;
        Some((1u32 << leading_zeros) - 1 + suffix)
    }

    /// `se(v)`: Exp-Golomb-coded signed integer, mapped from `ue(v)` (ITU-T H.264 §9.1.1).
    fn se(&mut self) -> Option<i32> {
        let code = self.ue()?;
        let magnitude = code.div_ceil(2);
        Some(if code % 2 == 0 {
            -(magnitude as i32)
        } else {
            magnitude as i32
        })
    }
}

/// `profile_idc` values ITU-T H.264 §7.3.2.1.1 requires an extra chroma/bit-depth block for,
/// between `seq_parameter_set_id` and `log2_max_frame_num_minus4`, that [`sps_ref_frame_info`]
/// does not implement — see its doc for why returning `None` for one of these is the right
/// response rather than a guess.
const SPS_PROFILES_WITH_CHROMA_BLOCK: [u8; 13] =
    [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135];

/// The reference-frame-related fields from an SPS's Exp-Golomb-coded body, past the fixed bytes
/// [`sps_profile`] reads: `max_num_ref_frames` (how many decoded pictures the encoder told the
/// decoder to keep around for prediction — the field this exists for) and `pic_order_cnt_type`
/// (which of three different, differently-shaped ways `max_num_ref_frames` is preceded by is
/// present — all three are walked correctly, including type 1's variable-length per-cycle
/// reference-offset list, since bailing out on one and silently mis-locating `max_num_ref_frames`
/// on the others would be a worse outcome than not parsing at all).
///
/// `None` in two cases, both treated the same way — "cannot be trusted, say nothing" rather than
/// guessing — since a wrong answer here is worse than no answer: `payload` too short to hold even
/// the fixed header `sps_profile` reads or to hold every field this parser walks past on its way
/// to `max_num_ref_frames`; and `profile_idc` is one of [`SPS_PROFILES_WITH_CHROMA_BLOCK`], which
/// this crate's encoder should never produce now that the profile is pinned to Constrained
/// Baseline (`crate::win::encode`), so seeing one anyway means the encoder drifted *again*,
/// differently, and guessing past a block this parser does not implement would silently misread
/// everything after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpsRefFrameInfo {
    pub max_num_ref_frames: u32,
    pub pic_order_cnt_type: u32,
}

pub fn sps_ref_frame_info(payload: &[u8]) -> Option<SpsRefFrameInfo> {
    let profile_idc = *payload.get(1)?;
    if SPS_PROFILES_WITH_CHROMA_BLOCK.contains(&profile_idc) {
        return None;
    }
    // Bytes 0-3 (header, profile_idc, constraint_flags, level_idc) are the fixed prefix
    // `sps_profile` already reads; everything from byte 4 on is Exp-Golomb-coded and must go
    // through emulation-prevention removal before any bit of it means anything.
    let rbsp = unescape_rbsp(payload.get(4..)?);
    let mut r = BitReader::new(&rbsp);

    let _seq_parameter_set_id = r.ue()?;
    let _log2_max_frame_num_minus4 = r.ue()?;
    let pic_order_cnt_type = r.ue()?;
    match pic_order_cnt_type {
        0 => {
            let _log2_max_pic_order_cnt_lsb_minus4 = r.ue()?;
        }
        1 => {
            let _delta_pic_order_always_zero_flag = r.u(1)?;
            let _offset_for_non_ref_pic = r.se()?;
            let _offset_for_top_to_bottom_field = r.se()?;
            let num_ref_frames_in_pic_order_cnt_cycle = r.ue()?;
            // Spec-legal range is 0..=255 (ITU-T H.264 §7.4.2.1.1); a larger value is not
            // something a real encoder emits, and looping on it anyway would be exactly the
            // kind of guess this parser exists to avoid, not a defensive nicety.
            if num_ref_frames_in_pic_order_cnt_cycle > 255 {
                return None;
            }
            for _ in 0..num_ref_frames_in_pic_order_cnt_cycle {
                let _offset_for_ref_frame = r.se()?;
            }
        }
        _ => {} // type 2: no additional fields at this point in the syntax.
    }
    let max_num_ref_frames = r.ue()?;

    Some(SpsRefFrameInfo {
        max_num_ref_frames,
        pic_order_cnt_type,
    })
}

/// [`sps_ref_frame_info`] for the first SPS NAL found in a raw Annex-B access unit, if any —
/// the bitstream-parsing counterpart to [`first_sps_profile`].
pub fn first_sps_ref_frame_info(raw: &[u8]) -> Option<SpsRefFrameInfo> {
    split_annexb(raw)
        .into_iter()
        .find(|nal| nal.kind == nal_type::SPS)
        .and_then(|nal| sps_ref_frame_info(nal.payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal, syntactically-fake but structurally-valid-for-testing NAL: just a
    /// header byte encoding `kind`, plus a byte of filler so it is not confused with an empty
    /// payload. Real NAL content does not matter to this module — it only routes and reorders
    /// whole units by type.
    fn nal(kind: u8, filler: u8) -> Vec<u8> {
        vec![kind & 0x1F, filler]
    }

    fn annexb(nals: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        for n in nals {
            out.extend_from_slice(&START_CODE);
            out.extend_from_slice(n);
        }
        out
    }

    #[test]
    fn split_accepts_both_start_code_lengths() {
        let mut raw = Vec::new();
        raw.extend_from_slice(&[0, 0, 1]); // 3-byte start code
        raw.extend_from_slice(&nal(nal_type::SLICE, 0xAA));
        raw.extend_from_slice(&[0, 0, 0, 1]); // 4-byte start code
        raw.extend_from_slice(&nal(nal_type::SLICE, 0xBB));

        let nals = split_annexb(&raw);
        assert_eq!(nals.len(), 2);
        assert_eq!(nals[0].kind, nal_type::SLICE);
        assert_eq!(nals[0].payload, &[nal_type::SLICE, 0xAA][..]);
        assert_eq!(nals[1].payload, &[nal_type::SLICE, 0xBB][..]);
    }

    #[test]
    fn non_keyframe_pass_through_gets_a_4_byte_start_code() {
        let raw = annexb(&[nal(nal_type::SLICE, 1)]);
        let mut cached = ParamSets::default();
        let (out, is_idr) = reframe(&raw, &mut cached);

        assert!(!is_idr);
        assert_eq!(out, annexb(&[nal(nal_type::SLICE, 1)]));
    }

    #[test]
    fn keyframe_with_parameter_sets_reorders_to_spec_and_caches_them() {
        // Deliberately out of order and interleaved with something unrelated, to prove this is
        // an actual reorder and not a lucky pass-through.
        let raw = annexb(&[
            nal(nal_type::PPS, 2),
            nal(nal_type::SPS, 1),
            nal(nal_type::SLICE_IDR, 9),
        ]);
        let mut cached = ParamSets::default();
        let (out, is_idr) = reframe(&raw, &mut cached);

        assert!(is_idr);
        assert_eq!(
            out,
            annexb(&[
                nal(nal_type::SPS, 1),
                nal(nal_type::PPS, 2),
                nal(nal_type::SLICE_IDR, 9),
            ])
        );
        assert_eq!(cached.sps, nal(nal_type::SPS, 1));
        assert_eq!(cached.pps, nal(nal_type::PPS, 2));
    }

    #[test]
    fn keyframe_without_parameter_sets_is_backfilled_from_the_cache() {
        let mut cached = ParamSets {
            sps: nal(nal_type::SPS, 7),
            pps: nal(nal_type::PPS, 8),
        };
        // This access unit's raw output — as some encoders do after the very first IDR — omits
        // SPS/PPS entirely.
        let raw = annexb(&[nal(nal_type::SLICE_IDR, 3)]);
        let (out, is_idr) = reframe(&raw, &mut cached);

        assert!(is_idr);
        assert_eq!(
            out,
            annexb(&[
                nal(nal_type::SPS, 7),
                nal(nal_type::PPS, 8),
                nal(nal_type::SLICE_IDR, 3),
            ])
        );
    }

    #[test]
    fn a_keyframe_with_no_cache_and_no_parameter_sets_of_its_own_still_emits_the_slice() {
        // No prior keyframe has ever been seen. Cannot fabricate parameter sets from nothing,
        // but must not drop the picture either — see the comment in `reframe`.
        let mut cached = ParamSets::default();
        let raw = annexb(&[nal(nal_type::SLICE_IDR, 5)]);
        let (out, is_idr) = reframe(&raw, &mut cached);

        assert!(is_idr);
        assert_eq!(out, annexb(&[nal(nal_type::SLICE_IDR, 5)]));
    }

    #[test]
    fn a_non_idr_i_frame_is_not_a_keyframe() {
        // §9.1: KEYFRAME means IDR specifically. A plain SLICE NAL, even an intra-coded one at
        // the H.264 syntax level this module cannot see into, must not be reported as a
        // keyframe — this module only ever sees nal_unit_type, and SLICE (1) is exactly what a
        // non-IDR I-frame uses on the wire, indistinguishable here from a P-frame by design.
        let raw = annexb(&[nal(nal_type::SLICE, 4)]);
        let mut cached = ParamSets::default();
        let (_, is_idr) = reframe(&raw, &mut cached);
        assert!(!is_idr);
    }

    #[test]
    fn aud_and_sei_are_ordered_first_and_preserved() {
        let raw = annexb(&[
            nal(nal_type::SLICE_IDR, 1),
            nal(nal_type::SEI, 2),
            nal(nal_type::SPS, 3),
            nal(nal_type::AUD, 4),
            nal(nal_type::PPS, 5),
        ]);
        let mut cached = ParamSets::default();
        let (out, is_idr) = reframe(&raw, &mut cached);

        assert!(is_idr);
        assert_eq!(
            out,
            annexb(&[
                nal(nal_type::AUD, 4),
                nal(nal_type::SEI, 2),
                nal(nal_type::SPS, 3),
                nal(nal_type::PPS, 5),
                nal(nal_type::SLICE_IDR, 1),
            ])
        );
    }

    #[test]
    fn a_non_keyframe_never_carries_parameter_sets_even_if_present_in_the_raw_bytes() {
        // Should not happen from a real encoder, but if it did, a non-keyframe must not somehow
        // gain an SPS/PPS pair it has no business carrying (§9.1: "A non-keyframe FrameData
        // must not contain an SPS or PPS NAL"). Parameter sets present in a non-IDR access
        // unit's raw bytes still update the cache (they are the freshest known), they are just
        // not *emitted* here since this access unit is not a keyframe.
        let raw = annexb(&[
            nal(nal_type::SPS, 1),
            nal(nal_type::PPS, 2),
            nal(nal_type::SLICE, 3),
        ]);
        let mut cached = ParamSets::default();
        let (out, is_idr) = reframe(&raw, &mut cached);

        assert!(!is_idr);
        assert_eq!(out, annexb(&[nal(nal_type::SLICE, 3)]));
        assert_eq!(cached.sps, nal(nal_type::SPS, 1));
    }

    #[test]
    fn multi_slice_pictures_keep_every_slice() {
        let raw = annexb(&[nal(nal_type::SLICE_IDR, 1), nal(nal_type::SLICE_IDR, 2)]);
        let mut cached = ParamSets {
            sps: nal(nal_type::SPS, 9),
            pps: nal(nal_type::PPS, 9),
        };
        let (out, is_idr) = reframe(&raw, &mut cached);
        assert!(is_idr);
        assert_eq!(
            out,
            annexb(&[
                nal(nal_type::SPS, 9),
                nal(nal_type::PPS, 9),
                nal(nal_type::SLICE_IDR, 1),
                nal(nal_type::SLICE_IDR, 2),
            ])
        );
    }

    #[test]
    fn trailing_filler_rides_after_the_slice() {
        const FILLER: u8 = 12;
        let raw = annexb(&[nal(nal_type::SLICE, 1), nal(FILLER, 2)]);
        let mut cached = ParamSets::default();
        let (out, _) = reframe(&raw, &mut cached);
        assert_eq!(out, annexb(&[nal(nal_type::SLICE, 1), nal(FILLER, 2)]));
    }

    #[test]
    fn a_stream_with_no_start_code_reframes_to_nothing() {
        let mut cached = ParamSets::default();
        let (out, is_idr) = reframe(&[0xAB, 0xCD, 0xEF], &mut cached);
        assert!(out.is_empty());
        assert!(!is_idr);
    }

    #[test]
    fn nal_summary_reports_every_nal_in_order_with_its_length() {
        // Payload length includes the header byte, same convention as `Nal::payload`; `nal()`
        // above always builds a 2-byte payload (header + one filler byte) with `nal_ref_idc`
        // implicitly 0, since it masks `kind` to 5 bits and sets nothing above that.
        let raw = annexb(&[
            nal(nal_type::SPS, 1),
            nal(nal_type::PPS, 2),
            nal(nal_type::SLICE, 3),
        ]);
        assert_eq!(
            nal_summary(&raw),
            vec![
                (nal_type::SPS, 0, 2),
                (nal_type::PPS, 0, 2),
                (nal_type::SLICE, 0, 2),
            ]
        );
    }

    #[test]
    fn nal_summary_sees_a_stray_parameter_set_reframe_would_have_stripped() {
        // The whole reason `nal_summary` exists separately from `reframe`'s own output: this
        // access unit is not a keyframe, so `reframe` strips its SPS/PPS from what gets emitted
        // (see `a_non_keyframe_never_carries_parameter_sets_even_if_present_in_the_raw_bytes`),
        // but a diagnostic reading the raw bytes still needs to see that they were there.
        let raw = annexb(&[
            nal(nal_type::SPS, 1),
            nal(nal_type::PPS, 2),
            nal(nal_type::SLICE, 3),
        ]);
        let summary = nal_summary(&raw);
        assert!(summary.iter().any(|&(kind, _, _)| kind == nal_type::SPS));
    }

    #[test]
    fn nal_summary_extracts_a_nonzero_nal_ref_idc() {
        // `nal()` above always encodes `nal_ref_idc = 0`; build the header byte directly here to
        // prove extraction actually reads bits 6-5, not just returning a hardcoded 0. `ref_idc =
        // 3` (a reference picture) on a plain SLICE, and `ref_idc = 0` (disposable) on another —
        // exactly the field that tells the two apart in a real capture.
        let ref_slice = vec![(3 << 5) | nal_type::SLICE, 0xAA];
        let disposable_slice = vec![nal_type::SLICE, 0xBB]; // ref_idc = 0
        let raw = annexb(&[ref_slice, disposable_slice]);

        assert_eq!(
            nal_summary(&raw),
            vec![(nal_type::SLICE, 3, 2), (nal_type::SLICE, 0, 2)]
        );
    }

    #[test]
    fn nal_type_name_covers_every_type_reframe_switches_on() {
        assert_eq!(nal_type_name(nal_type::SLICE), "SLICE");
        assert_eq!(nal_type_name(nal_type::SLICE_IDR), "SLICE_IDR");
        assert_eq!(nal_type_name(nal_type::SEI), "SEI");
        assert_eq!(nal_type_name(nal_type::SPS), "SPS");
        assert_eq!(nal_type_name(nal_type::PPS), "PPS");
        assert_eq!(nal_type_name(nal_type::AUD), "AUD");
    }

    #[test]
    fn nal_type_name_does_not_panic_on_an_unknown_type() {
        assert_eq!(nal_type_name(31), "OTHER");
    }

    #[test]
    fn sps_profile_reads_the_three_fixed_bytes_after_the_nal_header() {
        // header, profile_idc=66 (Baseline), constraint_flags=0x40 (constraint_set1_flag,
        // i.e. Constrained Baseline), level_idc=31 (level 3.1).
        let payload = [nal_type::SPS, 66, 0x40, 31, 0xAB, 0xCD];
        assert_eq!(sps_profile(&payload), Some((66, 0x40, 31)));
    }

    #[test]
    fn sps_profile_is_none_for_a_payload_too_short_to_hold_all_three_fields() {
        assert_eq!(sps_profile(&[nal_type::SPS]), None);
        assert_eq!(sps_profile(&[nal_type::SPS, 66]), None);
        assert_eq!(sps_profile(&[nal_type::SPS, 66, 0x40]), None);
    }

    #[test]
    fn first_sps_profile_finds_the_sps_among_other_nals_in_an_access_unit() {
        let raw = annexb(&[
            nal(nal_type::AUD, 0),
            vec![nal_type::SPS, 100, 0x00, 41], // High profile, no constraint flags, level 4.1
            nal(nal_type::SLICE_IDR, 9),
        ]);
        assert_eq!(first_sps_profile(&raw), Some((100, 0x00, 41)));
    }

    #[test]
    fn first_sps_profile_is_none_when_the_access_unit_has_no_sps() {
        let raw = annexb(&[nal(nal_type::SLICE, 1)]);
        assert_eq!(first_sps_profile(&raw), None);
    }

    // --- unescape_rbsp ---

    #[test]
    fn unescape_rbsp_is_a_no_op_when_there_is_nothing_to_escape() {
        assert_eq!(
            unescape_rbsp(&[1, 2, 3, 0, 4, 0, 0, 4]),
            vec![1, 2, 3, 0, 4, 0, 0, 4]
        );
    }

    #[test]
    fn unescape_rbsp_drops_the_emulation_prevention_byte() {
        // `00 00 03` -> `00 00`: the `03` is not data, only a start-code guard.
        assert_eq!(unescape_rbsp(&[1, 0, 0, 3, 2]), vec![1, 0, 0, 2]);
    }

    #[test]
    fn unescape_rbsp_leaves_a_real_0x03_alone_when_it_is_not_after_two_zeros() {
        // Only one zero precedes the `03` here, so it is real data, not an emulation guard.
        assert_eq!(unescape_rbsp(&[1, 0, 3, 2]), vec![1, 0, 3, 2]);
    }

    #[test]
    fn unescape_rbsp_handles_two_emulation_sequences_back_to_back() {
        assert_eq!(
            unescape_rbsp(&[0, 0, 3, 0, 0, 3, 1]),
            vec![0, 0, 0, 0, 1],
            "each `00 00 03` collapses independently, in order"
        );
    }

    #[test]
    fn unescape_rbsp_resets_the_zero_run_after_stripping_so_it_does_not_double_count() {
        // `00 00 03 00 03`: the first `03` strips (after two zeros); the run then resets, so
        // the single zero before the second `03` is not enough to strip it too.
        assert_eq!(unescape_rbsp(&[0, 0, 3, 0, 3]), vec![0, 0, 0, 3]);
    }

    // --- BitReader ---

    /// Pack MSB-first bits into bytes, padding the final byte with zero bits — enough for a
    /// reader that only consumes exactly as many bits as a test asks it to.
    fn bits_to_bytes(bits: &[bool]) -> Vec<u8> {
        let mut out = vec![0u8; bits.len().div_ceil(8)];
        for (i, &bit) in bits.iter().enumerate() {
            if bit {
                out[i / 8] |= 1 << (7 - (i % 8));
            }
        }
        out
    }

    /// Exp-Golomb-encode `value` as `ue(v)` (ITU-T H.264 §9.1), appending its bits to `bits`.
    /// Test-only: the production code only ever needs to decode this, never emit it, but a
    /// writer this simple is far less error-prone as a test fixture than hand-computed bit
    /// patterns, and lets these tests cover many values instead of a few worked-by-hand ones.
    fn write_ue(bits: &mut Vec<bool>, value: u32) {
        let x = value + 1;
        let num_bits = 32 - x.leading_zeros();
        bits.extend(std::iter::repeat_n(false, (num_bits - 1) as usize));
        for i in (0..num_bits).rev() {
            bits.push(((x >> i) & 1) == 1);
        }
    }

    /// Exp-Golomb-encode `value` as `se(v)` (ITU-T H.264 §9.1.1) via [`write_ue`].
    fn write_se(bits: &mut Vec<bool>, value: i32) {
        let code_num = if value <= 0 {
            (-2 * i64::from(value)) as u32
        } else {
            (2 * i64::from(value) - 1) as u32
        };
        write_ue(bits, code_num);
    }

    #[test]
    fn bit_reader_u_reads_msb_first() {
        // 0b1011_0000
        let mut r = BitReader::new(&[0b1011_0000]);
        assert_eq!(r.u(4), Some(0b1011));
        assert_eq!(r.u(4), Some(0b0000));
    }

    #[test]
    fn bit_reader_u_is_none_past_the_end() {
        let mut r = BitReader::new(&[0xFF]);
        assert_eq!(r.u(8), Some(0xFF));
        assert_eq!(r.u(1), None);
    }

    #[test]
    fn ue_round_trips_through_write_ue_for_a_range_of_values() {
        for value in [
            0u32, 1, 2, 3, 4, 5, 6, 7, 15, 16, 100, 1000, 65535, 1_000_000,
        ] {
            let mut bits = Vec::new();
            write_ue(&mut bits, value);
            let bytes = bits_to_bytes(&bits);
            let mut r = BitReader::new(&bytes);
            assert_eq!(r.ue(), Some(value), "round trip failed for {value}");
        }
    }

    #[test]
    fn ue_matches_the_spec_table_for_small_code_nums() {
        // ITU-T H.264 Table 9-2: codeNum -> bit string.
        let cases: &[(u32, &[bool])] = &[
            (0, &[true]),
            (1, &[false, true, false]),
            (2, &[false, true, true]),
            (3, &[false, false, true, false, false]),
            (4, &[false, false, true, false, true]),
        ];
        for &(value, expected_bits) in cases {
            let mut bits = Vec::new();
            write_ue(&mut bits, value);
            assert_eq!(bits, expected_bits, "codeNum {value}");
        }
    }

    #[test]
    fn se_round_trips_through_write_se_for_a_range_of_values() {
        for value in [0i32, 1, -1, 2, -2, 3, -3, 100, -100, 1000, -1000] {
            let mut bits = Vec::new();
            write_se(&mut bits, value);
            let bytes = bits_to_bytes(&bits);
            let mut r = BitReader::new(&bytes);
            assert_eq!(r.se(), Some(value), "round trip failed for {value}");
        }
    }

    // --- sps_ref_frame_info ---

    /// Build a full, valid SPS NAL payload (header byte included) for `profile_idc = 66`
    /// (Baseline/Constrained Baseline — outside `SPS_PROFILES_WITH_CHROMA_BLOCK`, so no
    /// unimplemented chroma block precedes the fields under test) with the given field values.
    fn build_sps(
        pic_order_cnt_type: u32,
        log2_max_pic_order_cnt_lsb_minus4: Option<u32>,
        max_num_ref_frames: u32,
    ) -> Vec<u8> {
        let mut bits = Vec::new();
        write_ue(&mut bits, 0); // seq_parameter_set_id
        write_ue(&mut bits, 2); // log2_max_frame_num_minus4
        write_ue(&mut bits, pic_order_cnt_type);
        if pic_order_cnt_type == 0 {
            write_ue(&mut bits, log2_max_pic_order_cnt_lsb_minus4.unwrap());
        }
        write_ue(&mut bits, max_num_ref_frames);
        // Trailing padding well past everything this parser reads, so it never runs off the end
        // of the buffer while reading a field this test does care about.
        bits.extend(std::iter::repeat_n(false, 32));
        let mut payload = vec![nal_type::SPS, 66, 0x40, 31]; // header, profile, constraints, level
        payload.extend(bits_to_bytes(&bits));
        payload
    }

    #[test]
    fn sps_ref_frame_info_reads_max_num_ref_frames_for_pic_order_cnt_type_0() {
        let payload = build_sps(0, Some(4), 1);
        assert_eq!(
            sps_ref_frame_info(&payload),
            Some(SpsRefFrameInfo {
                max_num_ref_frames: 1,
                pic_order_cnt_type: 0,
            })
        );
    }

    #[test]
    fn sps_ref_frame_info_reads_max_num_ref_frames_for_pic_order_cnt_type_2() {
        // Type 2 has no extra field before max_num_ref_frames, unlike type 0's
        // log2_max_pic_order_cnt_lsb_minus4 — a real way this parser could misalign itself if
        // the `match` in `sps_ref_frame_info` had the wrong arm for this type.
        let payload = build_sps(2, None, 3);
        assert_eq!(
            sps_ref_frame_info(&payload),
            Some(SpsRefFrameInfo {
                max_num_ref_frames: 3,
                pic_order_cnt_type: 2,
            })
        );
    }

    #[test]
    fn sps_ref_frame_info_reports_a_nonzero_ref_count_correctly_too() {
        // Not just "did it find 1" — a real drift back to more reference frames must show up as
        // something other than 1 too.
        let payload = build_sps(0, Some(4), 4);
        assert_eq!(
            sps_ref_frame_info(&payload).map(|i| i.max_num_ref_frames),
            Some(4)
        );
    }

    /// Build a full SPS payload for `pic_order_cnt_type == 1`, whose per-cycle reference-offset
    /// list has `cycle_len` entries (each an arbitrary but fixed `se(v)` value) before
    /// `max_num_ref_frames`.
    fn build_sps_pic_order_type_1(cycle_len: u32, max_num_ref_frames: u32) -> Vec<u8> {
        let mut bits = Vec::new();
        write_ue(&mut bits, 0); // seq_parameter_set_id
        write_ue(&mut bits, 2); // log2_max_frame_num_minus4
        write_ue(&mut bits, 1); // pic_order_cnt_type = 1
        write_ue(&mut bits, 0); // delta_pic_order_always_zero_flag (u(1), value 0 encodes the same either way)
        write_se(&mut bits, -3); // offset_for_non_ref_pic
        write_se(&mut bits, 2); // offset_for_top_to_bottom_field
        write_ue(&mut bits, cycle_len); // num_ref_frames_in_pic_order_cnt_cycle
        for i in 0..cycle_len {
            write_se(&mut bits, if i % 2 == 0 { 1 } else { -1 }); // offset_for_ref_frame[i]
        }
        write_ue(&mut bits, max_num_ref_frames);
        bits.extend(std::iter::repeat_n(false, 32));
        let mut payload = vec![nal_type::SPS, 66, 0x40, 31];
        payload.extend(bits_to_bytes(&bits));
        payload
    }

    #[test]
    fn sps_ref_frame_info_reads_max_num_ref_frames_for_pic_order_cnt_type_1_with_an_empty_cycle() {
        let payload = build_sps_pic_order_type_1(0, 1);
        assert_eq!(
            sps_ref_frame_info(&payload),
            Some(SpsRefFrameInfo {
                max_num_ref_frames: 1,
                pic_order_cnt_type: 1,
            })
        );
    }

    #[test]
    fn sps_ref_frame_info_reads_max_num_ref_frames_for_pic_order_cnt_type_1_with_a_nonempty_cycle()
    {
        // The part of this parser most likely to misalign itself: a variable-length loop, bound
        // by a value read earlier in the same bitstream, has to be walked exactly `cycle_len`
        // times — not zero, not `cycle_len + 1` — or `max_num_ref_frames` lands on the wrong
        // bits entirely.
        let payload = build_sps_pic_order_type_1(5, 2);
        assert_eq!(
            sps_ref_frame_info(&payload),
            Some(SpsRefFrameInfo {
                max_num_ref_frames: 2,
                pic_order_cnt_type: 1,
            })
        );
    }

    #[test]
    fn sps_ref_frame_info_is_none_when_the_pic_order_cnt_cycle_length_exceeds_the_spec_limit() {
        // ITU-T H.264 §7.4.2.1.1 limits `num_ref_frames_in_pic_order_cnt_cycle` to 0..=255; a
        // larger value is not something a real encoder emits, so this parser refuses it rather
        // than looping on it.
        let mut bits = Vec::new();
        write_ue(&mut bits, 0);
        write_ue(&mut bits, 2);
        write_ue(&mut bits, 1); // pic_order_cnt_type = 1
        write_ue(&mut bits, 0);
        write_se(&mut bits, 0);
        write_se(&mut bits, 0);
        write_ue(&mut bits, 256); // one past the spec-legal maximum
        let mut payload = vec![nal_type::SPS, 66, 0x40, 31];
        payload.extend(bits_to_bytes(&bits));

        assert_eq!(sps_ref_frame_info(&payload), None);
    }

    #[test]
    fn sps_ref_frame_info_is_none_for_a_profile_with_the_unimplemented_chroma_block() {
        // profile_idc = 100 (High) is in `SPS_PROFILES_WITH_CHROMA_BLOCK`; this crate's encoder
        // should never produce it (pinned to Constrained Baseline), so seeing one is treated as
        // "cannot be trusted" rather than parsed as if the block were not there.
        let payload = build_sps(0, Some(4), 1);
        let mut high_profile_payload = payload.clone();
        high_profile_payload[1] = 100;
        assert_eq!(sps_ref_frame_info(&high_profile_payload), None);
    }

    #[test]
    fn sps_ref_frame_info_is_none_for_a_payload_too_short_to_hold_the_fixed_prefix() {
        assert_eq!(sps_ref_frame_info(&[nal_type::SPS, 66, 0x40]), None);
    }

    #[test]
    fn first_sps_ref_frame_info_finds_the_sps_among_other_nals() {
        let sps_payload = build_sps(0, Some(4), 1);
        let raw = annexb(&[
            nal(nal_type::AUD, 0),
            sps_payload,
            nal(nal_type::SLICE_IDR, 9),
        ]);
        assert_eq!(
            first_sps_ref_frame_info(&raw),
            Some(SpsRefFrameInfo {
                max_num_ref_frames: 1,
                pic_order_cnt_type: 0,
            })
        );
    }

    #[test]
    fn first_sps_ref_frame_info_is_none_when_the_access_unit_has_no_sps() {
        let raw = annexb(&[nal(nal_type::SLICE, 1)]);
        assert_eq!(first_sps_ref_frame_info(&raw), None);
    }
}
