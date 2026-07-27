//! Primitive encoders shared by every message body (`docs/design/OXPROTO.md` §6).

use oxrdp_pdu::{DecodeError, DecodeResult, EncodeError, EncodeResult, ReadCursor, WriteCursor};

/// On-wire size of a length-prefixed UTF-8 string.
pub fn string_size(s: &str) -> usize {
    2 + s.len()
}

/// Write a `u16` length followed by the UTF-8 bytes (no NUL terminator).
pub fn write_string(dst: &mut WriteCursor<'_>, s: &str, ctx: &'static str) -> EncodeResult<()> {
    let len = u16::try_from(s.len()).map_err(|_| EncodeError::FieldTooLarge {
        context: ctx,
        field: "string length",
    })?;
    dst.write_u16_le(len, ctx)?;
    dst.write_slice(s.as_bytes(), ctx)
}

/// Read a `u16`-length-prefixed UTF-8 string. Invalid UTF-8 is a decode error rather than
/// being silently replaced: a peer sending malformed text is a bug worth surfacing.
pub fn read_string(src: &mut ReadCursor<'_>, ctx: &'static str) -> DecodeResult<String> {
    let len = src.read_u16_le(ctx)? as usize;
    let bytes = src.read_slice(len, ctx)?;
    String::from_utf8(bytes.to_vec()).map_err(|_| DecodeError::InvalidField {
        context: ctx,
        field: "string",
        reason: "not valid UTF-8",
    })
}

/// Write a boolean as a single byte.
pub fn write_bool(dst: &mut WriteCursor<'_>, value: bool, ctx: &'static str) -> EncodeResult<()> {
    dst.write_u8(u8::from(value), ctx)
}

/// Read a boolean; any non-zero byte is true.
pub fn read_bool(src: &mut ReadCursor<'_>, ctx: &'static str) -> DecodeResult<bool> {
    Ok(src.read_u8(ctx)? != 0)
}

/// Write a `u32`-length-prefixed byte blob.
pub fn write_blob(dst: &mut WriteCursor<'_>, data: &[u8], ctx: &'static str) -> EncodeResult<()> {
    let len = u32::try_from(data.len()).map_err(|_| EncodeError::FieldTooLarge {
        context: ctx,
        field: "blob length",
    })?;
    dst.write_u32_le(len, ctx)?;
    dst.write_slice(data, ctx)
}

/// Read a `u32`-length-prefixed byte blob.
pub fn read_blob(src: &mut ReadCursor<'_>, ctx: &'static str) -> DecodeResult<Vec<u8>> {
    let len = src.read_u32_le(ctx)? as usize;
    Ok(src.read_slice(len, ctx)?.to_vec())
}

/// Write an `i32`.
pub fn write_i32(dst: &mut WriteCursor<'_>, v: i32, ctx: &'static str) -> EncodeResult<()> {
    dst.write_u32_le(v as u32, ctx)
}

/// Read an `i32`.
pub fn read_i32(src: &mut ReadCursor<'_>, ctx: &'static str) -> DecodeResult<i32> {
    Ok(src.read_u32_le(ctx)? as i32)
}

/// Write an `i16`.
pub fn write_i16(dst: &mut WriteCursor<'_>, v: i16, ctx: &'static str) -> EncodeResult<()> {
    dst.write_u16_le(v as u16, ctx)
}

/// Read an `i16`.
pub fn read_i16(src: &mut ReadCursor<'_>, ctx: &'static str) -> DecodeResult<i16> {
    Ok(src.read_u16_le(ctx)? as i16)
}

/// Write a `u64`.
pub fn write_u64(dst: &mut WriteCursor<'_>, v: u64, ctx: &'static str) -> EncodeResult<()> {
    dst.write_u32_le((v & 0xFFFF_FFFF) as u32, ctx)?;
    dst.write_u32_le((v >> 32) as u32, ctx)
}

/// Read a `u64`.
pub fn read_u64(src: &mut ReadCursor<'_>, ctx: &'static str) -> DecodeResult<u64> {
    let lo = src.read_u32_le(ctx)? as u64;
    let hi = src.read_u32_le(ctx)? as u64;
    Ok((hi << 32) | lo)
}
