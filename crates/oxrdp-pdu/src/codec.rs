//! The [`Decode`] / [`Encode`] traits and convenience helpers.
//!
//! Every PDU type in the workspace implements these. They are deliberately small and
//! IO-free: decoding consumes a [`ReadCursor`], encoding fills a [`WriteCursor`], and
//! nothing touches a socket.

use crate::cursor::{ReadCursor, WriteCursor};
use crate::error::{DecodeResult, EncodeResult};

/// A type that can be decoded from the wire.
///
/// The `'de` lifetime lets a PDU borrow from the input buffer (e.g. a payload slice)
/// instead of copying.
pub trait Decode<'de>: Sized {
    /// Decode `Self` from `src`, advancing the cursor past the consumed bytes.
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self>;
}

/// A type that can be encoded to the wire.
pub trait Encode {
    /// The exact number of bytes [`Encode::encode`] will write.
    ///
    /// Callers rely on this to size buffers, so it must match `encode` precisely.
    fn size(&self) -> usize;

    /// Encode `self` into `dst`, advancing the cursor past the written bytes.
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()>;
}

/// Decode a `T` from a complete byte slice.
///
/// Note: this does not require the whole slice to be consumed; trailing bytes (e.g. a
/// payload after a header) are left for the caller to read separately.
pub fn decode<'de, T: Decode<'de>>(bytes: &'de [u8]) -> DecodeResult<T> {
    let mut cursor = ReadCursor::new(bytes);
    T::decode(&mut cursor)
}

/// Encode a `T` into a freshly allocated `Vec` sized by [`Encode::size`].
pub fn encode_vec<T: Encode>(pdu: &T) -> EncodeResult<Vec<u8>> {
    let mut buf = vec![0u8; pdu.size()];
    let mut cursor = WriteCursor::new(&mut buf);
    pdu.encode(&mut cursor)?;
    Ok(buf)
}
