//! Async chunk IO for the oxproto protocol over a tokio byte stream.
//!
//! This layer is deliberately thin: it moves *chunks* and hands reassembly to
//! [`oxproto::Reassembler`]. Keeping fragmentation out of the transport is what lets a future
//! QUIC transport map channels onto independent streams without changing anything above it.

use std::io;

use oxproto::envelope::{fragment, ChunkHeader, Message as RawMessage, Reassembler};
use oxproto::{decode, Message, CHUNK_HEADER_LEN, MAX_CHUNK_PAYLOAD};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Send one protocol message, fragmenting it across as many chunks as needed.
///
/// `video_channel` is the channel assigned to the window whose frames are being sent; it is
/// ignored for every message that has a fixed channel.
pub async fn write_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    msg: &Message,
    video_channel: u16,
) -> io::Result<()> {
    let chunks = msg
        .to_chunks(video_channel)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    for chunk in &chunks {
        writer.write_all(chunk).await?;
    }
    writer.flush().await
}

/// Send a pre-encoded body without constructing a [`Message`].
///
/// The frame hot path uses this: an encoder already owns the bitstream, and moving it through
/// an owned `Message` would copy a multi-megabyte buffer for nothing.
pub async fn write_raw<W: AsyncWrite + Unpin>(
    writer: &mut W,
    msg_type: u8,
    channel: u16,
    body: &[u8],
) -> io::Result<()> {
    let chunks = fragment(msg_type, channel, body)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    for chunk in &chunks {
        writer.write_all(chunk).await?;
    }
    writer.flush().await
}

/// Read chunks until a complete message is reassembled, then decode it.
///
/// Returns `Ok(None)` for a message type this build does not implement — the caller should
/// skip it and read again. That is what lets the protocol grow without a version break.
pub async fn read_message<R: AsyncRead + Unpin>(
    reader: &mut R,
    reassembler: &mut Reassembler,
) -> io::Result<Option<Message>> {
    let raw = read_reassembled(reader, reassembler).await?;
    Message::decode_known(raw.msg_type, &raw.payload)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

/// Read chunks until one complete message is reassembled, returning it undecoded.
pub async fn read_reassembled<R: AsyncRead + Unpin>(
    reader: &mut R,
    reassembler: &mut Reassembler,
) -> io::Result<RawMessage> {
    let mut header_buf = [0u8; CHUNK_HEADER_LEN];
    let mut payload = Vec::new();
    loop {
        reader.read_exact(&mut header_buf).await?;
        // Decoding validates the reserved flag bits and the MAX_CHUNK_PAYLOAD bound, so the
        // allocation below is bounded by a constant no matter what the peer claims.
        let header: ChunkHeader = decode(&header_buf)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        debug_assert!(header.length as usize <= MAX_CHUNK_PAYLOAD);

        payload.clear();
        payload.resize(header.length as usize, 0);
        reader.read_exact(&mut payload).await?;

        match reassembler.push(&header, &payload) {
            Ok(Some(msg)) => return Ok(msg),
            Ok(None) => continue,
            Err(e) => return Err(io::Error::new(io::ErrorKind::InvalidData, e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxproto::envelope::channel;
    use oxproto::message::{ClientHello, DisplayLayout, FrameData, KeyEvent, Output, Ping};

    fn hello() -> Message {
        Message::ClientHello(ClientHello {
            version_min: 1,
            version_max: 1,
            features: 0b11,
            auth_token: "token".into(),
            client_name: "oxclient".into(),
            codecs: vec![1],
            display: DisplayLayout {
                outputs: vec![Output {
                    id: 0,
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                    scale_num: 1,
                    scale_den: 1,
                    refresh_mhz: 60_000,
                }],
            },
        })
    }

    #[tokio::test]
    async fn round_trips_small_messages() {
        let (mut a, mut b) = tokio::io::duplex(4096);
        let mut r = Reassembler::new();

        let h = hello();
        write_message(&mut a, &h, channel::CONTROL).await.unwrap();
        assert_eq!(read_message(&mut b, &mut r).await.unwrap(), Some(h));

        let p = Message::Ping(Ping {
            seq: 3,
            sent_us: 42,
        });
        write_message(&mut a, &p, channel::CONTROL).await.unwrap();
        assert_eq!(read_message(&mut b, &mut r).await.unwrap(), Some(p));
    }

    #[tokio::test]
    async fn round_trips_a_fragmented_frame() {
        // Larger than one chunk, so this exercises fragmentation + reassembly over the wire.
        let payload = vec![0x5Au8; MAX_CHUNK_PAYLOAD * 2 + 11];
        let frame = Message::FrameData(FrameData {
            window_id: 1,
            frame_id: 7,
            codec: 1,
            flags: 1,
            width: 800,
            height: 600,
            captured_us: 10,
            encoded_us: 20,
            data: payload,
        });

        let (mut a, mut b) = tokio::io::duplex(64 * 1024);
        let expected = frame.clone();
        let writer = tokio::spawn(async move {
            write_message(&mut a, &frame, channel::VIDEO_BASE)
                .await
                .unwrap();
        });

        let mut r = Reassembler::new();
        let got = read_message(&mut b, &mut r).await.unwrap().unwrap();
        writer.await.unwrap();
        assert_eq!(got, expected);
    }

    #[tokio::test]
    async fn interleaved_channels_do_not_corrupt_each_other() {
        let (mut a, mut b) = tokio::io::duplex(128 * 1024);

        // Write the first half of a fragmented frame, a complete key event, then the rest.
        let body = vec![1u8; MAX_CHUNK_PAYLOAD + 5];
        let chunks = fragment(oxproto::msg_type::FRAME_DATA, channel::VIDEO_BASE, &body).unwrap();
        let key = Message::KeyEvent(KeyEvent {
            scancode: 0x1E,
            flags: 1,
            timestamp: 5,
        });

        a.write_all(&chunks[0]).await.unwrap();
        write_message(&mut a, &key, channel::VIDEO_BASE)
            .await
            .unwrap();
        a.write_all(&chunks[1]).await.unwrap();
        a.flush().await.unwrap();

        let mut r = Reassembler::new();
        // The key event completes first even though the frame started first — this is the
        // head-of-line property the channel design exists for.
        assert_eq!(read_message(&mut b, &mut r).await.unwrap(), Some(key));
        let frame = read_reassembled(&mut b, &mut r).await.unwrap();
        assert_eq!(frame.msg_type, oxproto::msg_type::FRAME_DATA);
        assert_eq!(frame.payload, body);
    }

    #[tokio::test]
    async fn unknown_message_type_is_skipped_not_fatal() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        // 0x7F is unassigned; the envelope's length lets a receiver step over it.
        write_raw(&mut a, 0x7F, channel::CONTROL, &[1, 2, 3])
            .await
            .unwrap();
        let mut r = Reassembler::new();
        assert_eq!(read_message(&mut b, &mut r).await.unwrap(), None);
    }

    #[tokio::test]
    async fn rejects_a_bogus_chunk_header() {
        let (mut a, mut b) = tokio::io::duplex(64);
        // Reserved flag bits set.
        a.write_all(&[oxproto::msg_type::PING, 0x80, 0, 0, 0, 0, 0, 0])
            .await
            .unwrap();
        a.flush().await.unwrap();
        let mut r = Reassembler::new();
        assert!(read_message(&mut b, &mut r).await.is_err());
    }
}
