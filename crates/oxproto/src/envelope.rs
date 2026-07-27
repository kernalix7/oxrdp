//! Chunk framing: the 8-byte header, fragmentation, reassembly, and size limits.
//!
//! See `docs/design/OXPROTO.md` §3–§4 and §16. Three properties matter here:
//!
//! 1. **`length` is authoritative.** A body is decoded from exactly the slice the header
//!    declares, so a malformed body can never read into the next message.
//! 2. **Big messages are fragmented** onto a channel, capped at [`MAX_CHUNK_PAYLOAD`], so a
//!    keyframe cannot delay input or control — the head-of-line blocking this protocol exists
//!    to avoid.
//! 3. **Nothing is allocated on a peer's say-so.** Per-type caps are checked against the
//!    header before any buffer grows, and reassembly buffers grow with arriving data rather
//!    than pre-allocating the declared size.

use std::collections::HashMap;

use oxrdp_pdu::{Decode, DecodeError, DecodeResult, Encode, EncodeResult, ReadCursor, WriteCursor};

use crate::message::msg_type;

/// Bytes in a chunk header.
pub const CHUNK_HEADER_LEN: usize = 8;

/// Largest payload carried by a single chunk. This is a latency bound: the longest a
/// higher-priority channel can be delayed by a lower-priority one.
pub const MAX_CHUNK_PAYLOAD: usize = 32 * 1024;

/// Well-known channel numbers (`docs/design/OXPROTO.md` §4).
pub mod channel {
    /// Handshake, ping/pong, errors, quality hints, display layout.
    pub const CONTROL: u16 = 0;
    /// Pointer, keyboard, text, window control (client → agent).
    pub const INPUT: u16 = 1;
    /// Cursor shape/position/visibility (agent → client).
    pub const CURSOR: u16 = 2;
    /// Window lifecycle events (agent → client).
    pub const WINDOW: u16 = 3;
    /// First channel available for per-window video streams.
    pub const VIDEO_BASE: u16 = 16;
}

/// Chunk header flag bits.
pub mod chunk_flags {
    /// More chunks follow for this message on this channel.
    pub const FRAG_MORE: u8 = 0x01;
    /// Bits that are defined; anything else must be zero.
    pub const KNOWN: u8 = FRAG_MORE;
}

/// The 8-byte chunk header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkHeader {
    /// Message type (see [`crate::message::msg_type`]), repeated on every chunk.
    pub msg_type: u8,
    /// Flag bits ([`chunk_flags`]).
    pub flags: u8,
    /// Logical channel ([`channel`]).
    pub channel: u16,
    /// Payload bytes in *this* chunk.
    pub length: u32,
}

impl ChunkHeader {
    /// Whether more chunks follow for this message.
    pub fn has_more(&self) -> bool {
        self.flags & chunk_flags::FRAG_MORE != 0
    }
}

impl Encode for ChunkHeader {
    fn size(&self) -> usize {
        CHUNK_HEADER_LEN
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u8(self.msg_type, "chunk type")?;
        dst.write_u8(self.flags, "chunk flags")?;
        dst.write_u16_le(self.channel, "chunk channel")?;
        dst.write_u32_le(self.length, "chunk length")?;
        Ok(())
    }
}

impl<'de> Decode<'de> for ChunkHeader {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        let msg_type = src.read_u8("chunk type")?;
        let flags = src.read_u8("chunk flags")?;
        let channel = src.read_u16_le("chunk channel")?;
        let length = src.read_u32_le("chunk length")?;

        // Reserved bits are *ignored*, not rejected: a sender must clear them, but a
        // receiver that hard-failed on an unknown bit would break the day a future version
        // starts using one — the opposite of the forward compatibility §17 promises. New
        // behaviour is gated by feature negotiation, so ignoring an unnegotiated bit is safe.
        let flags = flags & chunk_flags::KNOWN;
        if length as usize > MAX_CHUNK_PAYLOAD {
            return Err(DecodeError::InvalidLength {
                context: "oxproto chunk",
                reason: "chunk payload exceeds MAX_CHUNK_PAYLOAD",
            });
        }
        Ok(Self {
            msg_type,
            flags,
            channel,
            length,
        })
    }
}

/// Most channels that may hold a partially reassembled message at once.
///
/// Reassembly state is allocated *before* authentication (the handshake itself arrives through
/// it), so without this a peer could open partial fragment sequences on thousands of distinct
/// channel ids and pin memory without ever presenting a token. Legitimate traffic needs only a
/// handful: control, input, cursor, window, and one video channel per shared window.
pub const MAX_PENDING_CHANNELS: usize = 64;

/// Total bytes buffered across all partially reassembled messages.
///
/// The per-type limit bounds one message; this bounds the sum, so many channels each holding a
/// legal-but-large partial message cannot add up to an unbounded total.
pub const MAX_PENDING_BYTES: usize = 64 * 1024 * 1024;

/// Maximum size of a fully reassembled message of this type (`OXPROTO.md` §16).
///
/// Checked while reassembling, before the buffer is allowed to grow, so a peer cannot make a
/// receiver allocate arbitrary memory.
pub fn max_message_len(msg_type: u8) -> usize {
    const KIB: usize = 1024;
    const MIB: usize = 1024 * KIB;
    match msg_type {
        msg_type::WINDOW_OPENED
        | msg_type::WINDOW_TITLE
        | msg_type::DISPLAY_LAYOUT
        | msg_type::ERROR => 8 * KIB,
        msg_type::CURSOR_SHAPE => 256 * KIB,
        msg_type::WINDOW_ICON => MIB,
        msg_type::FRAME_DATA => 32 * MIB,
        // Handshake, control, input and the small cursor messages.
        _ => 4 * KIB,
    }
}

/// Split `payload` into chunk-sized frames, returning the complete wire bytes.
///
/// A zero-length payload still produces one chunk, so a body-less message is representable.
pub fn fragment(msg_type: u8, channel: u16, payload: &[u8]) -> EncodeResult<Vec<Vec<u8>>> {
    let mut out = Vec::new();
    let mut offset = 0usize;
    loop {
        let end = (offset + MAX_CHUNK_PAYLOAD).min(payload.len());
        let piece = &payload[offset..end];
        let more = end < payload.len();
        let header = ChunkHeader {
            msg_type,
            flags: if more { chunk_flags::FRAG_MORE } else { 0 },
            channel,
            length: piece.len() as u32,
        };

        let mut buf = vec![0u8; CHUNK_HEADER_LEN + piece.len()];
        {
            let mut cursor = WriteCursor::new(&mut buf);
            header.encode(&mut cursor)?;
            cursor.write_slice(piece, "chunk payload")?;
        }
        out.push(buf);

        offset = end;
        if !more {
            break;
        }
    }
    Ok(out)
}

/// Per-channel reassembly of fragmented messages.
///
/// A channel carries at most one message at a time; a chunk whose type disagrees with the
/// message already in flight on that channel is a protocol error.
#[derive(Debug, Default)]
pub struct Reassembler {
    pending: HashMap<u16, Pending>,
    pending_bytes: usize,
}

#[derive(Debug)]
struct Pending {
    msg_type: u8,
    buf: Vec<u8>,
}

impl Reassembler {
    /// A reassembler with no messages in flight.
    pub fn new() -> Self {
        Self::default()
    }

    /// How many channels currently have a partial message.
    pub fn pending_channels(&self) -> usize {
        self.pending.len()
    }

    /// Bytes currently buffered across all partially reassembled messages.
    pub fn pending_bytes(&self) -> usize {
        self.pending_bytes
    }

    /// Feed one chunk. Returns the complete `(msg_type, payload)` once the final chunk of a
    /// message arrives, or `None` while more chunks are expected.
    pub fn push(&mut self, header: &ChunkHeader, payload: &[u8]) -> DecodeResult<Option<Message>> {
        if payload.len() != header.length as usize {
            return Err(DecodeError::InvalidLength {
                context: "oxproto reassembly",
                reason: "payload length does not match the chunk header",
            });
        }

        // Fast path: an unfragmented message with nothing pending on the channel.
        if !header.has_more() && !self.pending.contains_key(&header.channel) {
            check_len(header.msg_type, payload.len())?;
            return Ok(Some(Message {
                msg_type: header.msg_type,
                payload: payload.to_vec(),
            }));
        }

        let is_new_channel = !self.pending.contains_key(&header.channel);
        if is_new_channel && self.pending.len() >= MAX_PENDING_CHANNELS {
            return Err(DecodeError::InvalidLength {
                context: "oxproto reassembly",
                reason: "too many channels with a partially reassembled message",
            });
        }
        if self.pending_bytes + payload.len() > MAX_PENDING_BYTES {
            return Err(DecodeError::InvalidLength {
                context: "oxproto reassembly",
                reason: "total buffered reassembly bytes exceeded",
            });
        }

        let entry = self
            .pending
            .entry(header.channel)
            .or_insert_with(|| Pending {
                msg_type: header.msg_type,
                buf: Vec::new(),
            });
        if entry.msg_type != header.msg_type {
            let dropped = self.pending.remove(&header.channel);
            self.pending_bytes -= dropped.map(|p| p.buf.len()).unwrap_or(0);
            return Err(DecodeError::InvalidField {
                context: "oxproto reassembly",
                field: "type",
                reason: "chunk type differs from the message in flight on this channel",
            });
        }

        // Grow with the data that actually arrived — never to a declared size.
        if let Err(e) = check_len(header.msg_type, entry.buf.len() + payload.len()) {
            let dropped = self.pending.remove(&header.channel);
            self.pending_bytes -= dropped.map(|p| p.buf.len()).unwrap_or(0);
            return Err(e);
        }
        entry.buf.extend_from_slice(payload);
        self.pending_bytes += payload.len();

        if header.has_more() {
            return Ok(None);
        }
        // NOTE: the wire format cannot distinguish "final chunk of the sequence in flight" from
        // "a fresh unfragmented message that happens to share this channel and type" — a sender
        // that abandons a fragment sequence and reuses the channel gets its leftover bytes
        // spliced onto the next message. That is a sender bug, and it gains a hostile peer
        // nothing it could not achieve by concatenating the bytes itself.
        let done = self
            .pending
            .remove(&header.channel)
            .expect("entry was just inserted or updated");
        self.pending_bytes -= done.buf.len();
        Ok(Some(Message {
            msg_type: done.msg_type,
            payload: done.buf,
        }))
    }
}

fn check_len(msg_type: u8, len: usize) -> DecodeResult<()> {
    if len > max_message_len(msg_type) {
        return Err(DecodeError::InvalidLength {
            context: "oxproto reassembly",
            reason: "message exceeds the maximum size for its type",
        });
    }
    Ok(())
}

/// A fully reassembled message: its type and its body bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// Message type code.
    pub msg_type: u8,
    /// Complete body, ready to decode.
    pub payload: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxrdp_pdu::{decode, encode_vec};

    fn parse(frame: &[u8]) -> (ChunkHeader, Vec<u8>) {
        let header = decode::<ChunkHeader>(frame).unwrap();
        let body = frame[CHUNK_HEADER_LEN..][..header.length as usize].to_vec();
        (header, body)
    }

    #[test]
    fn header_round_trip() {
        let h = ChunkHeader {
            msg_type: msg_type::FRAME_DATA,
            flags: chunk_flags::FRAG_MORE,
            channel: channel::VIDEO_BASE,
            length: 1234,
        };
        let bytes = encode_vec(&h).unwrap();
        assert_eq!(bytes.len(), CHUNK_HEADER_LEN);
        assert_eq!(bytes[0], msg_type::FRAME_DATA);
        assert_eq!(bytes[1], chunk_flags::FRAG_MORE);
        assert_eq!(&bytes[2..4], &channel::VIDEO_BASE.to_le_bytes());
        assert_eq!(&bytes[4..8], &1234u32.to_le_bytes());
        assert_eq!(decode::<ChunkHeader>(&bytes).unwrap(), h);
        assert!(h.has_more());
    }

    #[test]
    fn reserved_flags_are_ignored_not_rejected() {
        // Forward compatibility: a future version may define one of these bits, and a current
        // receiver must not hard-fail the connection over it.
        let bytes = [
            msg_type::PING,
            0x80 | chunk_flags::FRAG_MORE,
            0,
            0,
            0,
            0,
            0,
            0,
        ];
        let h = decode::<ChunkHeader>(&bytes).unwrap();
        assert_eq!(
            h.flags,
            chunk_flags::FRAG_MORE,
            "unknown bits are masked off"
        );
        assert!(h.has_more());
    }

    #[test]
    fn caps_the_number_of_pending_channels() {
        // A peer must not be able to pin memory on thousands of channels before authenticating.
        let mut r = Reassembler::new();
        let chunk = vec![0u8; 16];
        for ch in 0..MAX_PENDING_CHANNELS as u16 {
            let h = ChunkHeader {
                msg_type: msg_type::FRAME_DATA,
                flags: chunk_flags::FRAG_MORE,
                channel: channel::VIDEO_BASE + ch,
                length: chunk.len() as u32,
            };
            assert!(r.push(&h, &chunk).unwrap().is_none());
        }
        assert_eq!(r.pending_channels(), MAX_PENDING_CHANNELS);

        let overflow = ChunkHeader {
            msg_type: msg_type::FRAME_DATA,
            flags: chunk_flags::FRAG_MORE,
            channel: 60000,
            length: chunk.len() as u32,
        };
        assert!(matches!(
            r.push(&overflow, &chunk),
            Err(DecodeError::InvalidLength { .. })
        ));
    }

    #[test]
    fn pending_bytes_are_tracked_and_released() {
        let mut r = Reassembler::new();
        let body = vec![3u8; MAX_CHUNK_PAYLOAD + 1];
        let frames = fragment(msg_type::FRAME_DATA, channel::VIDEO_BASE, &body).unwrap();

        let (h0, b0) = parse(&frames[0]);
        r.push(&h0, &b0).unwrap();
        assert_eq!(r.pending_bytes(), MAX_CHUNK_PAYLOAD);

        let (h1, b1) = parse(&frames[1]);
        r.push(&h1, &b1).unwrap().expect("completes");
        assert_eq!(
            r.pending_bytes(),
            0,
            "a completed message releases its buffer"
        );
        assert_eq!(r.pending_channels(), 0);
    }

    #[test]
    fn rejects_oversized_chunk() {
        let mut bytes = [0u8; CHUNK_HEADER_LEN];
        bytes[0] = msg_type::FRAME_DATA;
        bytes[4..8].copy_from_slice(&((MAX_CHUNK_PAYLOAD + 1) as u32).to_le_bytes());
        assert!(matches!(
            decode::<ChunkHeader>(&bytes),
            Err(DecodeError::InvalidLength { .. })
        ));
    }

    #[test]
    fn small_message_is_one_chunk() {
        let frames = fragment(msg_type::PING, channel::CONTROL, &[1, 2, 3]).unwrap();
        assert_eq!(frames.len(), 1);
        let (h, body) = parse(&frames[0]);
        assert!(!h.has_more());
        let mut r = Reassembler::new();
        let msg = r.push(&h, &body).unwrap().unwrap();
        assert_eq!(msg.msg_type, msg_type::PING);
        assert_eq!(msg.payload, vec![1, 2, 3]);
        assert_eq!(r.pending_channels(), 0);
    }

    #[test]
    fn empty_payload_still_produces_a_chunk() {
        let frames = fragment(msg_type::PING, channel::CONTROL, &[]).unwrap();
        assert_eq!(frames.len(), 1);
        let (h, body) = parse(&frames[0]);
        assert_eq!(h.length, 0);
        let mut r = Reassembler::new();
        assert_eq!(
            r.push(&h, &body).unwrap().unwrap().payload,
            Vec::<u8>::new()
        );
    }

    #[test]
    fn large_message_fragments_and_reassembles() {
        let payload: Vec<u8> = (0..(MAX_CHUNK_PAYLOAD * 2 + 7)).map(|i| i as u8).collect();
        let frames = fragment(msg_type::FRAME_DATA, channel::VIDEO_BASE, &payload).unwrap();
        assert_eq!(frames.len(), 3);

        let mut r = Reassembler::new();
        let mut out = None;
        for (i, f) in frames.iter().enumerate() {
            let (h, body) = parse(f);
            assert_eq!(h.has_more(), i < 2);
            if let Some(msg) = r.push(&h, &body).unwrap() {
                out = Some(msg);
            }
        }
        let msg = out.expect("message completed");
        assert_eq!(msg.msg_type, msg_type::FRAME_DATA);
        assert_eq!(msg.payload, payload);
        assert_eq!(r.pending_channels(), 0);
    }

    #[test]
    fn channels_reassemble_independently() {
        let video: Vec<u8> = vec![7u8; MAX_CHUNK_PAYLOAD + 1];
        let video_frames = fragment(msg_type::FRAME_DATA, channel::VIDEO_BASE, &video).unwrap();
        let input_frames = fragment(msg_type::KEY_EVENT, channel::INPUT, &[9, 9]).unwrap();

        let mut r = Reassembler::new();
        // First video chunk, then a complete input message interleaved, then the rest.
        let (h0, b0) = parse(&video_frames[0]);
        assert!(r.push(&h0, &b0).unwrap().is_none());

        let (hi, bi) = parse(&input_frames[0]);
        let input = r
            .push(&hi, &bi)
            .unwrap()
            .expect("input completes immediately");
        assert_eq!(input.msg_type, msg_type::KEY_EVENT);

        let (h1, b1) = parse(&video_frames[1]);
        let done = r.push(&h1, &b1).unwrap().expect("video completes");
        assert_eq!(done.payload, video);
    }

    #[test]
    fn rejects_type_change_mid_message() {
        let frames = fragment(
            msg_type::FRAME_DATA,
            channel::VIDEO_BASE,
            &vec![0u8; MAX_CHUNK_PAYLOAD + 1],
        )
        .unwrap();
        let mut r = Reassembler::new();
        let (h0, b0) = parse(&frames[0]);
        r.push(&h0, &b0).unwrap();

        let bogus = ChunkHeader {
            msg_type: msg_type::PING,
            flags: 0,
            channel: channel::VIDEO_BASE,
            length: 1,
        };
        assert!(matches!(
            r.push(&bogus, &[0]),
            Err(DecodeError::InvalidField { field: "type", .. })
        ));
        assert_eq!(r.pending_channels(), 0, "the failed channel is discarded");
    }

    #[test]
    fn rejects_message_over_its_type_limit() {
        // PING's limit is 4 KiB; feed fragments past it.
        let mut r = Reassembler::new();
        let chunk = vec![0u8; MAX_CHUNK_PAYLOAD];
        let h = ChunkHeader {
            msg_type: msg_type::PING,
            flags: chunk_flags::FRAG_MORE,
            channel: channel::CONTROL,
            length: chunk.len() as u32,
        };
        assert!(matches!(
            r.push(&h, &chunk),
            Err(DecodeError::InvalidLength { .. })
        ));
        assert_eq!(r.pending_channels(), 0);
    }

    #[test]
    fn rejects_payload_length_mismatch() {
        let h = ChunkHeader {
            msg_type: msg_type::PING,
            flags: 0,
            channel: channel::CONTROL,
            length: 4,
        };
        let mut r = Reassembler::new();
        assert!(matches!(
            r.push(&h, &[1, 2]),
            Err(DecodeError::InvalidLength { .. })
        ));
    }
}
