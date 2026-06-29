use crate::cursor::{ReadCursor, WriteCursor};
use crate::error::{DecodeError, DecodeResult, EncodeError, EncodeResult};

/// Number of bytes `write_length` will emit for `len`.
pub fn length_size(len: usize) -> usize {
    if len < 0x80 {
        1
    } else {
        1 + len.to_be_bytes().iter().skip_while(|&&b| b == 0).count()
    }
}

/// Write a BER definite length.
pub fn write_length(dst: &mut WriteCursor<'_>, len: usize, ctx: &'static str) -> EncodeResult<()> {
    if len < 0x80 {
        dst.write_u8(len as u8, ctx)?;
    } else {
        let be = len.to_be_bytes();
        let stripped = be
            .iter()
            .skip_while(|&&b| b == 0)
            .copied()
            .collect::<Vec<u8>>();
        if stripped.len() > 4 {
            return Err(EncodeError::FieldTooLarge {
                context: ctx,
                field: "ber length",
            });
        }
        dst.write_u8(0x80 | stripped.len() as u8, ctx)?;
        dst.write_slice(&stripped, ctx)?;
    }
    Ok(())
}

/// Read a BER definite length. Rejects the indefinite form (0x80) and lengths whose
/// byte-count exceeds 4 (`DecodeError::InvalidLength`).
pub fn read_length(src: &mut ReadCursor<'_>, ctx: &'static str) -> DecodeResult<usize> {
    let b = src.read_u8(ctx)?;
    if b < 0x80 {
        return Ok(usize::from(b));
    }
    if b == 0x80 {
        return Err(DecodeError::InvalidLength {
            context: ctx,
            reason: "indefinite length not allowed",
        });
    }
    let n = b & 0x7F;
    if n > 4 {
        return Err(DecodeError::InvalidLength {
            context: ctx,
            reason: "length too large",
        });
    }
    let mut len = 0usize;
    for _ in 0..n {
        len = (len << 8) | usize::from(src.read_u8(ctx)?);
    }
    Ok(len)
}

/// Write a BER BOOLEAN TLV: tag 0x01, length 0x01, value 0xFF (true) or 0x00 (false).
pub fn write_boolean(
    dst: &mut WriteCursor<'_>,
    value: bool,
    ctx: &'static str,
) -> EncodeResult<()> {
    dst.write_u8(0x01, ctx)?;
    dst.write_u8(0x01, ctx)?;
    dst.write_u8(if value { 0xFF } else { 0x00 }, ctx)?;
    Ok(())
}

/// Read a BER BOOLEAN TLV (tag 0x01, len 0x01). Any nonzero value is `true`.
pub fn read_boolean(src: &mut ReadCursor<'_>, ctx: &'static str) -> DecodeResult<bool> {
    let tag = src.read_u8(ctx)?;
    if tag != 0x01 {
        return Err(DecodeError::InvalidField {
            context: ctx,
            field: "ber boolean tag",
            reason: "unexpected tag",
        });
    }
    let len = src.read_u8(ctx)?;
    if len != 0x01 {
        return Err(DecodeError::InvalidField {
            context: ctx,
            field: "ber boolean length",
            reason: "unexpected length",
        });
    }
    Ok(src.read_u8(ctx)? != 0)
}

/// Read a one-byte tag and the following length, verifying the tag equals `expected_tag`
/// (a 1- or 2-byte slice). Returns the content length. Errors `InvalidField` on tag mismatch.
pub fn read_tag_length(
    src: &mut ReadCursor<'_>,
    expected_tag: &[u8],
    ctx: &'static str,
) -> DecodeResult<usize> {
    for &expected in expected_tag {
        let actual = src.read_u8(ctx)?;
        if actual != expected {
            return Err(DecodeError::InvalidField {
                context: ctx,
                field: "ber tag",
                reason: "unexpected tag",
            });
        }
    }
    read_length(src, ctx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc_len(len: usize) -> Vec<u8> {
        let mut buf = vec![0u8; length_size(len)];
        let mut c = WriteCursor::new(&mut buf);
        write_length(&mut c, len, "test").unwrap();
        buf
    }

    #[test]
    fn lengths_round_trip() {
        for (len, bytes) in [
            (0usize, vec![0x00]),
            (5, vec![0x05]),
            (127, vec![0x7F]),
            (128, vec![0x81, 0x80]),
            (200, vec![0x81, 0xC8]),
            (300, vec![0x82, 0x01, 0x2C]),
            (65535, vec![0x82, 0xFF, 0xFF]),
        ] {
            assert_eq!(enc_len(len), bytes, "encode {len}");
            let mut c = ReadCursor::new(&bytes);
            assert_eq!(read_length(&mut c, "t").unwrap(), len, "decode {len}");
        }
    }

    #[test]
    fn boolean_round_trip() {
        let mut buf = [0u8; 3];
        let mut c = WriteCursor::new(&mut buf);
        write_boolean(&mut c, true, "t").unwrap();
        assert_eq!(buf, [0x01, 0x01, 0xFF]);
        let mut r = ReadCursor::new(&buf);
        assert!(read_boolean(&mut r, "t").unwrap());
    }

    #[test]
    fn rejects_indefinite_length() {
        let mut c = ReadCursor::new(&[0x80]);
        assert!(matches!(
            read_length(&mut c, "t"),
            Err(DecodeError::InvalidLength { .. })
        ));
    }

    #[test]
    fn tag_length_reads_content_len() {
        // application tag 101 = [0x7F, 0x65], then long-form length 300
        let buf = [0x7F, 0x65, 0x82, 0x01, 0x2C];
        let mut c = ReadCursor::new(&buf);
        assert_eq!(read_tag_length(&mut c, &[0x7F, 0x65], "t").unwrap(), 300);
    }
}
