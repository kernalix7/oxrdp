//! Error types for PDU encode/decode.
//!
//! Every variant is recoverable: a malformed or truncated server message yields one of
//! these errors, never a panic. This is the core memory-safety property of the decoder.

use std::fmt;

/// An error decoding a PDU from the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// The buffer ended before a field could be fully read.
    NotEnoughBytes {
        /// What was being read when the buffer ran out.
        context: &'static str,
        /// Bytes required to read the field.
        needed: usize,
        /// Bytes actually remaining in the buffer.
        remaining: usize,
    },
    /// A field held a value outside its allowed set.
    InvalidField {
        /// The PDU being decoded.
        context: &'static str,
        /// The offending field.
        field: &'static str,
        /// Why the value was rejected.
        reason: &'static str,
    },
    /// A length/size field was inconsistent with the buffer or the protocol.
    InvalidLength {
        /// The PDU being decoded.
        context: &'static str,
        /// Why the length was rejected.
        reason: &'static str,
    },
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::NotEnoughBytes {
                context,
                needed,
                remaining,
            } => write!(
                f,
                "{context}: not enough bytes (need {needed}, have {remaining})"
            ),
            DecodeError::InvalidField {
                context,
                field,
                reason,
            } => write!(f, "{context}: invalid field `{field}` ({reason})"),
            DecodeError::InvalidLength { context, reason } => {
                write!(f, "{context}: invalid length ({reason})")
            }
        }
    }
}

impl std::error::Error for DecodeError {}

/// An error encoding a PDU to the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodeError {
    /// The destination buffer was too small for the PDU.
    NotEnoughSpace {
        /// What was being written when space ran out.
        context: &'static str,
        /// Bytes required to write the field.
        needed: usize,
        /// Bytes of space actually remaining.
        remaining: usize,
    },
    /// A value did not fit the width of its on-wire field.
    FieldTooLarge {
        /// The PDU being encoded.
        context: &'static str,
        /// The offending field.
        field: &'static str,
    },
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EncodeError::NotEnoughSpace {
                context,
                needed,
                remaining,
            } => write!(
                f,
                "{context}: not enough space (need {needed}, have {remaining})"
            ),
            EncodeError::FieldTooLarge { context, field } => {
                write!(
                    f,
                    "{context}: field `{field}` too large for its on-wire width"
                )
            }
        }
    }
}

impl std::error::Error for EncodeError {}

/// Result of a decode operation.
pub type DecodeResult<T> = Result<T, DecodeError>;

/// Result of an encode operation.
pub type EncodeResult<T> = Result<T, EncodeError>;
