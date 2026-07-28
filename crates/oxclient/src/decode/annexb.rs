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
