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
}
