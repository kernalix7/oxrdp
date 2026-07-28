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
///
/// # Cancellation
///
/// **This function is not cancel-safe.** Its read progress lives in local variables, so a future
/// dropped part-way through a chunk takes the bytes it already consumed with it, and the next
/// read resumes in the middle of a chunk — where the length field is whatever the payload
/// happened to contain. The symptom is a bogus "chunk payload exceeds MAX_CHUNK_PAYLOAD" some
/// time later, far from the cancellation that caused it.
///
/// Do not call this inside `tokio::select!`. Use [`ChunkReader`], which keeps the same progress
/// in the caller's own state and therefore resumes correctly, or read on a dedicated task and
/// hand messages over a channel.
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

/// Where a partially-read chunk left off.
#[derive(Debug)]
enum ReadState {
    /// Filling the 8-byte chunk header; `filled` bytes are already in place.
    Header { filled: usize },
    /// Header parsed; filling its payload, of which `filled` bytes are already in place.
    Payload { header: ChunkHeader, filled: usize },
}

/// A cancel-safe chunk reader.
///
/// Same job as [`read_reassembled`], with one difference that matters: every byte of progress is
/// held here rather than in the future, so dropping [`ChunkReader::next_message`] part-way
/// through a chunk loses nothing and the next call picks up exactly where it stopped. That is
/// what makes it safe to read inside a `tokio::select!` alongside, say, a channel of input
/// events to send.
///
/// It is built on [`AsyncReadExt::read`], which tokio documents as cancel-safe (a cancelled
/// `read` has consumed nothing); `read_exact` carries no such guarantee, which is precisely the
/// problem this type exists to avoid.
#[derive(Debug)]
pub struct ChunkReader {
    header_buf: [u8; CHUNK_HEADER_LEN],
    payload: Vec<u8>,
    state: ReadState,
}

impl Default for ChunkReader {
    fn default() -> Self {
        Self::new()
    }
}

impl ChunkReader {
    /// A reader positioned at the start of a chunk header.
    pub fn new() -> Self {
        Self {
            header_buf: [0u8; CHUNK_HEADER_LEN],
            payload: Vec::new(),
            state: ReadState::Header { filled: 0 },
        }
    }

    /// Read chunks until `reassembler` yields one complete message.
    ///
    /// Cancel-safe: if the returned future is dropped, the bytes already read stay in `self`.
    pub async fn next_message<R: AsyncRead + Unpin>(
        &mut self,
        reader: &mut R,
        reassembler: &mut Reassembler,
    ) -> io::Result<RawMessage> {
        loop {
            match self.state {
                ReadState::Header { mut filled } => {
                    while filled < CHUNK_HEADER_LEN {
                        let n = reader.read(&mut self.header_buf[filled..]).await?;
                        if n == 0 {
                            // Record the progress made before EOF so a caller that retries after
                            // a reconnect is not silently resumed mid-header.
                            self.state = ReadState::Header { filled };
                            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
                        }
                        filled += n;
                        self.state = ReadState::Header { filled };
                    }
                    // Decoding validates the reserved flag bits and the MAX_CHUNK_PAYLOAD bound,
                    // so the allocation below is bounded by a constant no matter what the peer
                    // claims.
                    let header: ChunkHeader = decode(&self.header_buf)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
                    debug_assert!(header.length as usize <= MAX_CHUNK_PAYLOAD);
                    self.payload.clear();
                    self.payload.resize(header.length as usize, 0);
                    self.state = ReadState::Payload { header, filled: 0 };
                }
                ReadState::Payload { header, mut filled } => {
                    while filled < self.payload.len() {
                        let n = reader.read(&mut self.payload[filled..]).await?;
                        if n == 0 {
                            self.state = ReadState::Payload { header, filled };
                            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
                        }
                        filled += n;
                        self.state = ReadState::Payload { header, filled };
                    }
                    self.state = ReadState::Header { filled: 0 };
                    match reassembler.push(&header, &self.payload) {
                        Ok(Some(msg)) => return Ok(msg),
                        Ok(None) => continue,
                        Err(e) => {
                            return Err(io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
                        }
                    }
                }
            }
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

    /// A frame large enough to span several chunks (`MAX_CHUNK_PAYLOAD` is 32 KiB), so the test
    /// below cancels in the middle of a *fragmented* message and not merely a single chunk.
    fn big_frame() -> Message {
        Message::FrameData(FrameData {
            window_id: 7,
            frame_id: 11,
            codec: 1,
            flags: 0,
            width: 320,
            height: 64,
            captured_us: 5,
            encoded_us: 9,
            data: (0..80_000u32).map(|i| (i % 251) as u8).collect(),
        })
    }

    fn encoded(msg: &Message) -> Vec<u8> {
        msg.to_chunks(channel::VIDEO_BASE).unwrap().concat()
    }

    #[tokio::test]
    async fn a_cancelled_read_resumes_where_it_stopped() {
        use std::time::Duration;
        use tokio::time::timeout;

        let msg = big_frame();
        let bytes = encoded(&msg);
        // Room for the whole message, so the writes below never block on the reader.
        let (mut a, mut b) = tokio::io::duplex(bytes.len() + 1024);

        let mut reader = ChunkReader::new();
        let mut reassembler = Reassembler::new();

        // Deliver the message in awkward pieces: 3 bytes stops part-way through the very first
        // header, and the later cuts land deep inside the payload of a fragmented chunk. After
        // each piece, a `timeout` that expires drops the read future mid-chunk — exactly the way
        // a `tokio::select!` branch is dropped when its sibling wins.
        let mut sent = 0usize;
        for cut in [3, bytes.len() / 3, bytes.len() - 7] {
            a.write_all(&bytes[sent..cut]).await.unwrap();
            a.flush().await.unwrap();
            sent = cut;
            assert!(
                timeout(
                    Duration::from_millis(20),
                    reader.next_message(&mut b, &mut reassembler)
                )
                .await
                .is_err(),
                "an incomplete message must not resolve"
            );
        }
        a.write_all(&bytes[sent..]).await.unwrap();
        a.flush().await.unwrap();

        // Every cancellation above dropped a future that had already consumed bytes. If that
        // progress had lived in the future rather than in `ChunkReader`, those bytes would be
        // gone and this read would resume mid-chunk, decode a payload byte as a length, and fail.
        let raw = timeout(
            Duration::from_secs(1),
            reader.next_message(&mut b, &mut reassembler),
        )
        .await
        .expect("the message is complete by now")
        .expect("reassembly must succeed");

        assert_eq!(raw.msg_type, msg.msg_type());
        let decoded = Message::decode_known(raw.msg_type, &raw.payload)
            .unwrap()
            .unwrap();
        assert_eq!(decoded, msg);
    }

    #[tokio::test]
    async fn chunk_reader_agrees_with_read_reassembled_when_uninterrupted() {
        let msg = big_frame();
        let bytes = encoded(&msg);

        let (mut a, mut b) = tokio::io::duplex(bytes.len() + 1024);
        a.write_all(&bytes).await.unwrap();
        a.flush().await.unwrap();
        let mut reassembler = Reassembler::new();
        let via_reader = ChunkReader::new()
            .next_message(&mut b, &mut reassembler)
            .await
            .unwrap();

        let (mut c, mut d) = tokio::io::duplex(bytes.len() + 1024);
        c.write_all(&bytes).await.unwrap();
        c.flush().await.unwrap();
        let mut reassembler = Reassembler::new();
        let via_fn = read_reassembled(&mut d, &mut reassembler).await.unwrap();

        assert_eq!(via_reader.msg_type, via_fn.msg_type);
        assert_eq!(via_reader.payload, via_fn.payload);
    }
}
