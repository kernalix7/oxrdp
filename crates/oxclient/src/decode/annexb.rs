//! Just enough Annex-B parsing to tell whether a payload can start a stream.
//!
//! The client cannot rely on `frame_flag::KEYFRAME` alone. It is set by the agent, and an agent
//! that mislabels one frame would either hand the decoder inter-coded data with no reference
//! picture (garbage on screen) or strand a client that joined mid-stream forever. Reading the NAL
//! headers is a dozen lines and makes the decision from the bitstream itself.
//!
//! The scan looks for an IDR slice specifically, matching `OXPROTO.md` §9.1: an access unit can
//! be intra-coded without being an IDR, and starting there guarantees nothing, so "any intra
//! frame" is exactly the relaxation that must not be made. Parameter sets are not accepted as a
//! start signal either — §9.1 has them accompany the IDR in the same access unit, so an SPS
//! without an IDR beside it is not a picture and clearing the gate on one would let the *next*
//! frame reach the decoder with no reference picture.

/// Coded slice of an IDR picture.
const NAL_IDR: u8 = 5;

/// Whether an Annex-B payload carries an IDR slice, and can therefore start a decode.
///
/// Both start code forms are accepted, as `OXPROTO.md` §9.1 requires: the agent emits the 4-byte
/// `00 00 00 01`, but a general-purpose Annex-B demuxer produces the 3-byte `00 00 01` and
/// rejecting that buys nothing. NAL types with no bearing on the question — SEI, access unit
/// delimiters, filler, anything unrecognised — are skipped rather than treated as an error.
#[must_use]
pub fn contains_idr(data: &[u8]) -> bool {
    let mut index = 0;
    // A NAL needs a 3-byte start code plus one header byte to be worth looking at. The 4-byte
    // start code contains the 3-byte form at its second byte, so scanning for the short form
    // finds both.
    while index + 3 < data.len() {
        if data[index] == 0 && data[index + 1] == 0 && data[index + 2] == 1 {
            let header = data[index + 3];
            // forbidden_zero_bit must be 0; anything else is not a NAL header.
            if header & 0x80 == 0 && header & 0x1f == NAL_IDR {
                return true;
            }
            index += 4;
        } else {
            index += 1;
        }
    }
    false
}

/// Sequence parameter set.
const NAL_SPS: u8 = 7;
/// Picture parameter set.
const NAL_PPS: u8 = 8;

/// Every NAL unit in an Annex-B payload: its type, and its length in bytes with the header
/// included and the start code excluded.
///
/// Allocates, so this is for diagnostics rather than the decode path.
#[must_use]
pub fn units(data: &[u8]) -> Vec<(u8, usize)> {
    let mut starts = Vec::new();
    let mut index = 0;
    while index + 3 < data.len() {
        if data[index] == 0 && data[index + 1] == 0 && data[index + 2] == 1 {
            starts.push(index + 3);
            index += 4;
        } else {
            index += 1;
        }
    }

    starts
        .iter()
        .enumerate()
        .map(|(position, &start)| {
            // A unit runs to the start code of the next one. That start code is three bytes,
            // with a fourth leading zero when the encoder used the long form, and both have to
            // be excluded or every length is reported three or four bytes too long.
            let end = starts.get(position + 1).map_or(data.len(), |&next| {
                let short = next.saturating_sub(3);
                if short > start && data[short - 1] == 0 {
                    short - 1
                } else {
                    short
                }
            });
            (data[start] & 0x1f, end.saturating_sub(start))
        })
        .collect()
}

/// Whether an Annex-B payload carries a sequence or picture parameter set.
///
/// `OXPROTO.md` §9.1 allows these only on a keyframe, so a non-keyframe for which this is true
/// is an encoder bug rather than something a decoder should accommodate.
#[must_use]
pub fn has_parameter_sets(data: &[u8]) -> bool {
    units(data)
        .into_iter()
        .any(|(nal_type, _)| nal_type == NAL_SPS || nal_type == NAL_PPS)
}

/// A one-line summary of what an access unit contains: NAL types in order, with lengths.
///
/// This is what turns "the decoder was unhappy" into "the decoder was unhappy about *this*".
/// A codec error code says only that something was rejected; the shape of the access unit is
/// what says whether the encoder sent something it should not have.
#[must_use]
pub fn describe(data: &[u8]) -> String {
    let units = units(data);
    if units.is_empty() {
        return format!("no NAL units in {} bytes", data.len());
    }
    units
        .into_iter()
        .map(|(nal_type, len)| format!("{}({len})", name(nal_type)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The H.264 name for a NAL unit type, for the ones worth naming.
fn name(nal_type: u8) -> String {
    match nal_type {
        1 => "slice".to_string(),
        2..=4 => format!("partition-{nal_type}"),
        5 => "IDR".to_string(),
        6 => "SEI".to_string(),
        7 => "SPS".to_string(),
        8 => "PPS".to_string(),
        9 => "AUD".to_string(),
        10 => "end-of-sequence".to_string(),
        11 => "end-of-stream".to_string(),
        12 => "filler".to_string(),
        13 => "SPS-extension".to_string(),
        14 => "prefix".to_string(),
        15 => "subset-SPS".to_string(),
        19 => "auxiliary-slice".to_string(),
        20 => "slice-extension".to_string(),
        other => format!("type-{other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_an_idr_slice_after_either_start_code() {
        assert!(contains_idr(&[0, 0, 1, 0x65, 0x88, 0x84]));
        assert!(contains_idr(&[0, 0, 0, 1, 0x65, 0x88, 0x84]));
    }

    #[test]
    fn finds_the_idr_in_a_keyframe_access_unit() {
        // The layout OXPROTO.md §9.1 mandates for a keyframe: SPS, then PPS, then the slice.
        let access_unit = [
            0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1e, // SPS
            0, 0, 0, 1, 0x68, 0xce, 0x3c, 0x80, // PPS
            0, 0, 0, 1, 0x65, 0x88, 0x84, // IDR slice
        ];
        assert!(contains_idr(&access_unit));
    }

    #[test]
    fn skips_nal_types_it_has_no_use_for() {
        // The full keyframe layout OXPROTO.md §9.1 mandates: an access unit delimiter first if
        // present, then SEI, then the parameter sets, then the slice, then anything else.
        let access_unit = [
            0, 0, 0, 1, 0x09, 0x10, // AUD
            0, 0, 0, 1, 0x06, 0x05, 0x01, 0x00, // SEI
            0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1e, // SPS
            0, 0, 0, 1, 0x68, 0xce, 0x3c, 0x80, // PPS
            0, 0, 0, 1, 0x65, 0x88, // IDR slice
            0, 0, 0, 1, 0x0c, 0xff, 0x80, // filler
        ];
        assert!(contains_idr(&access_unit));
    }

    #[test]
    fn finds_an_idr_wherever_it_sits() {
        // §9.1 pins the order, but the scan does not depend on it: a decoder that only accepts
        // the layout it expects turns an encoder's ordering bug into a window that never
        // starts, which is a worse failure than tolerating the bug.
        let out_of_order = [
            0, 0, 0, 1, 0x0c, 0xff, 0x80, // filler ahead of everything
            0, 0, 0, 1, 0x65, 0x88, // IDR slice
            0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1e, // SPS after the slice
        ];
        assert!(contains_idr(&out_of_order));
    }

    #[test]
    fn rejects_a_payload_with_no_idr_slice() {
        // A non-IDR slice (type 1) cannot start a decode...
        assert!(!contains_idr(&[0, 0, 0, 1, 0x41, 0x9a, 0x00]));
        // ...and neither can parameter sets on their own: §9.1 keeps them in the same access
        // unit as the IDR, so a payload holding only these is not a picture to start from.
        let parameter_sets = [
            0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1e, // SPS
            0, 0, 0, 1, 0x68, 0xce, 0x3c, 0x80, // PPS
        ];
        assert!(!contains_idr(&parameter_sets));
    }

    #[test]
    fn units_report_each_nal_type_and_length() {
        // A keyframe in the layout §9.1 mandates. Lengths are the NAL unit itself: header byte
        // included, start code excluded. Getting that wrong by the three or four bytes of the
        // next start code is the obvious way for this to be quietly useless.
        let access_unit = [
            0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1e, // SPS, 4 bytes
            0, 0, 0, 1, 0x68, 0xce, 0x3c, // PPS, 3 bytes
            0, 0, 1, 0x65, 0x88, 0x84, 0x21, 0x11, // IDR, 5 bytes, short start code
        ];

        assert_eq!(units(&access_unit), vec![(7, 4), (8, 3), (5, 5)]);
    }

    #[test]
    fn describe_names_the_units_in_order() {
        let access_unit = [
            0, 0, 0, 1, 0x09, 0x10, // AUD
            0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1e, // SPS
            0, 0, 0, 1, 0x68, 0xce, 0x3c, // PPS
            0, 0, 0, 1, 0x65, 0x88, 0x84, // IDR
        ];

        assert_eq!(describe(&access_unit), "AUD(2) SPS(4) PPS(3) IDR(3)");
    }

    #[test]
    fn describe_says_so_when_there_is_nothing_to_describe() {
        // A payload with no start codes at all is itself the finding.
        assert_eq!(
            describe(&[0xde, 0xad, 0xbe, 0xef]),
            "no NAL units in 4 bytes"
        );
        assert_eq!(describe(&[]), "no NAL units in 0 bytes");
    }

    #[test]
    fn describe_names_unknown_types_rather_than_hiding_them() {
        assert_eq!(describe(&[0, 0, 0, 1, 0x18, 0x00]), "type-24(2)");
    }

    #[test]
    fn parameter_sets_are_detected_for_the_rule_that_forbids_them() {
        // §9.1 allows these only on a keyframe, so this is what makes an encoder that repeats
        // them on a delta frame visible rather than merely suspected.
        let with_sps = [0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x41, 0x9a];
        let with_pps = [0, 0, 0, 1, 0x68, 0xce, 0, 0, 0, 1, 0x41, 0x9a];
        let delta_only = [0, 0, 0, 1, 0x41, 0x9a, 0x00];

        assert!(has_parameter_sets(&with_sps));
        assert!(has_parameter_sets(&with_pps));
        assert!(!has_parameter_sets(&delta_only));
    }

    #[test]
    fn rejects_a_header_with_the_forbidden_bit_set() {
        assert!(!contains_idr(&[0, 0, 0, 1, 0xe5, 0x88]));
    }

    #[test]
    fn rejects_truncated_and_empty_payloads() {
        assert!(!contains_idr(&[]));
        assert!(!contains_idr(&[0, 0, 1]));
        assert!(!contains_idr(&[0x65, 0x88, 0x84]));
    }
}
