//! Bounds-checked read/write cursors over byte buffers.
//!
//! Unlike `bytes::Buf`, these never panic on a short buffer — every read is checked and
//! returns [`DecodeError::NotEnoughBytes`]. Server input is untrusted, so this is the only
//! cursor used for decoding.

use crate::error::{DecodeError, DecodeResult, EncodeError, EncodeResult};

/// A forward-only, bounds-checked reader over a byte slice.
pub struct ReadCursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> ReadCursor<'a> {
    /// Wrap a byte slice at position 0.
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Bytes not yet consumed.
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// Whether the cursor has consumed the whole buffer.
    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    /// Current read offset from the start of the buffer.
    pub fn position(&self) -> usize {
        self.pos
    }

    fn ensure(&self, n: usize, context: &'static str) -> DecodeResult<()> {
        if self.remaining() < n {
            Err(DecodeError::NotEnoughBytes {
                context,
                needed: n,
                remaining: self.remaining(),
            })
        } else {
            Ok(())
        }
    }

    /// Read a fixed-size array.
    pub fn read_array<const N: usize>(&mut self, context: &'static str) -> DecodeResult<[u8; N]> {
        self.ensure(N, context)?;
        let mut out = [0u8; N];
        out.copy_from_slice(&self.buf[self.pos..self.pos + N]);
        self.pos += N;
        Ok(out)
    }

    /// Borrow the next `n` bytes without copying.
    pub fn read_slice(&mut self, n: usize, context: &'static str) -> DecodeResult<&'a [u8]> {
        self.ensure(n, context)?;
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    /// Read a `u8`.
    pub fn read_u8(&mut self, context: &'static str) -> DecodeResult<u8> {
        Ok(self.read_array::<1>(context)?[0])
    }

    /// Read a big-endian `u16`.
    pub fn read_u16_be(&mut self, context: &'static str) -> DecodeResult<u16> {
        Ok(u16::from_be_bytes(self.read_array::<2>(context)?))
    }

    /// Read a little-endian `u16`.
    pub fn read_u16_le(&mut self, context: &'static str) -> DecodeResult<u16> {
        Ok(u16::from_le_bytes(self.read_array::<2>(context)?))
    }

    /// Read a big-endian `u32`.
    pub fn read_u32_be(&mut self, context: &'static str) -> DecodeResult<u32> {
        Ok(u32::from_be_bytes(self.read_array::<4>(context)?))
    }

    /// Read a little-endian `u32`.
    pub fn read_u32_le(&mut self, context: &'static str) -> DecodeResult<u32> {
        Ok(u32::from_le_bytes(self.read_array::<4>(context)?))
    }

    /// Look at the next `u8` without consuming it.
    pub fn peek_u8(&self, context: &'static str) -> DecodeResult<u8> {
        self.ensure(1, context)?;
        Ok(self.buf[self.pos])
    }
}

/// A forward-only, bounds-checked writer over a mutable byte slice.
pub struct WriteCursor<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> WriteCursor<'a> {
    /// Wrap a mutable byte slice at position 0.
    pub fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Space not yet written.
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// Current write offset from the start of the buffer.
    pub fn position(&self) -> usize {
        self.pos
    }

    fn ensure(&self, n: usize, context: &'static str) -> EncodeResult<()> {
        if self.remaining() < n {
            Err(EncodeError::NotEnoughSpace {
                context,
                needed: n,
                remaining: self.remaining(),
            })
        } else {
            Ok(())
        }
    }

    /// Write a raw byte slice.
    pub fn write_slice(&mut self, src: &[u8], context: &'static str) -> EncodeResult<()> {
        self.ensure(src.len(), context)?;
        self.buf[self.pos..self.pos + src.len()].copy_from_slice(src);
        self.pos += src.len();
        Ok(())
    }

    /// Write a `u8`.
    pub fn write_u8(&mut self, value: u8, context: &'static str) -> EncodeResult<()> {
        self.write_slice(&[value], context)
    }

    /// Write a big-endian `u16`.
    pub fn write_u16_be(&mut self, value: u16, context: &'static str) -> EncodeResult<()> {
        self.write_slice(&value.to_be_bytes(), context)
    }

    /// Write a little-endian `u16`.
    pub fn write_u16_le(&mut self, value: u16, context: &'static str) -> EncodeResult<()> {
        self.write_slice(&value.to_le_bytes(), context)
    }

    /// Write a big-endian `u32`.
    pub fn write_u32_be(&mut self, value: u32, context: &'static str) -> EncodeResult<()> {
        self.write_slice(&value.to_be_bytes(), context)
    }

    /// Write a little-endian `u32`.
    pub fn write_u32_le(&mut self, value: u32, context: &'static str) -> EncodeResult<()> {
        self.write_slice(&value.to_le_bytes(), context)
    }
}
