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
///
/// # Cancellation
///
/// **This function is not cancel-safe.** It is built on [`AsyncWriteExt::write_all`], which
/// tokio documents as not cancel-safe: if the returned future is dropped part-way through a
/// chunk, some prefix of that chunk has already gone out on the wire, but nothing records how
/// much. This is worse than an interrupted read — the peer is not just missing bytes, it has
/// *received* a truncated chunk, so whatever is written next (by this call or an unrelated one)
/// lands right after it and is misread as more of that chunk's payload. There is no local
/// recovery: the peer's framing is desynchronised for the rest of the connection.
///
/// Do not call this inside `tokio::select!`, `tokio::time::timeout`, or anywhere else the
/// future can be dropped before it resolves. Use [`ChunkWriter`], which keeps write progress in
/// the caller's own state and resumes correctly, exactly the way [`ChunkReader`] does for reads.
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
///
/// # Cancellation
///
/// **Not cancel-safe**, for the same reason as [`write_message`] — see its documentation. Use
/// [`ChunkWriter`] anywhere this call might be dropped before it resolves.
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

/// A cancel-safe chunked-message writer.
///
/// [`write_message`] and [`write_raw`] are built on [`AsyncWriteExt::write_all`], which is not
/// cancel-safe: a dropped `write_all` may have already put a prefix of the chunk on the wire
/// with no way for the caller to know how much, so whatever is written next lands right after
/// it and the peer reads it as more of the old chunk's payload — the framing desynchronises for
/// the rest of the connection, worse than an interrupted read because the corruption is on the
/// wire, not just in a local buffer.
///
/// `ChunkWriter` avoids this the same way [`ChunkReader`] does for reads: progress lives in
/// `self`, not in the write future, using [`AsyncWriteExt::write`], which tokio documents as
/// cancel-safe (a cancelled `write` has written nothing it did not already report through its
/// `Ok` return).
///
/// Queueing and flushing are separate on purpose. [`Self::queue_message`] / [`Self::queue_raw`]
/// only append to an internal buffer — a synchronous operation that cannot be interrupted
/// part-way — while [`Self::flush`] is the only part that touches the socket and is safe to
/// drop mid-write. To resume a cancelled write, call `flush` again; **do not** call
/// `queue_message`/`queue_raw` again for the same message, or it is queued (and eventually
/// sent) a second time. Queueing a genuinely different message while a previous one is still
/// only partially flushed is fine — it is appended after it and both go out in order.
///
/// ```no_run
/// # use oxtransport::ChunkWriter;
/// # use oxproto::message::{Message, Ping};
/// # async fn example<W: tokio::io::AsyncWrite + Unpin>(io: &mut W) -> std::io::Result<()> {
/// let mut writer = ChunkWriter::new();
/// writer.queue_message(&Message::Ping(Ping { seq: 1, sent_us: 0 }), 0)?;
/// writer.flush(io).await?; // if this is dropped before resolving, call `flush` again to resume
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Default)]
pub struct ChunkWriter {
    /// Every byte queued for the peer that has not yet been fully written and flushed.
    pending: Vec<u8>,
    /// How many bytes of `pending`, from the front, are already on the wire.
    written: usize,
}

impl ChunkWriter {
    /// A writer with nothing queued.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether anything is queued and not yet fully flushed.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Queue a message's chunks. Synchronous and cannot be interrupted part-way.
    pub fn queue_message(&mut self, msg: &Message, video_channel: u16) -> io::Result<()> {
        let chunks = msg
            .to_chunks(video_channel)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        for chunk in &chunks {
            self.pending.extend_from_slice(chunk);
        }
        Ok(())
    }

    /// Queue a pre-encoded body's chunks. Synchronous and cannot be interrupted part-way.
    pub fn queue_raw(&mut self, msg_type: u8, channel: u16, body: &[u8]) -> io::Result<()> {
        let chunks = fragment(msg_type, channel, body)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        for chunk in &chunks {
            self.pending.extend_from_slice(chunk);
        }
        Ok(())
    }

    /// Write out everything queued so far.
    ///
    /// Cancel-safe: if the returned future is dropped, the bytes already written stay recorded
    /// in `self`, and the next call to `flush` resumes mid-buffer instead of re-sending or
    /// splicing an unrelated write into the middle of this one.
    pub async fn flush<W: AsyncWrite + Unpin>(&mut self, writer: &mut W) -> io::Result<()> {
        while self.written < self.pending.len() {
            let n = writer.write(&self.pending[self.written..]).await?;
            if n == 0 {
                return Err(io::Error::from(io::ErrorKind::WriteZero));
            }
            self.written += n;
        }
        self.pending.clear();
        self.written = 0;
        writer.flush().await
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

    /// A single-chunk message whose encoding is comfortably bigger than the tiny duplex
    /// capacity the write-cancellation tests below use, so the very first `write` cannot
    /// possibly finish it in one call.
    fn one_chunk_message() -> Message {
        Message::FrameData(FrameData {
            window_id: 7,
            frame_id: 11,
            codec: 1,
            flags: 0,
            width: 320,
            height: 64,
            captured_us: 5,
            encoded_us: 9,
            data: vec![0xAB; 50],
        })
    }

    /// Demonstrates the hazard [`ChunkWriter`] exists to avoid. `write_message` is built on
    /// `write_all`, which tokio documents as not cancel-safe: dropping it part-way through a
    /// chunk leaves an unknown prefix already on the wire. A caller with no way to know how much
    /// went out does the only thing it can — sends the next message fresh — and that lands right
    /// after the stray bytes. The peer, still expecting the rest of the first (much larger)
    /// declared payload, consumes the second message as more of it instead of ever recognising
    /// it as its own message, and the connection ends in an EOF that has nothing to do with
    /// what actually went wrong.
    #[tokio::test]
    async fn a_cancelled_write_all_corrupts_the_stream() {
        use std::time::Duration;
        use tokio::time::timeout;

        let big = one_chunk_message();
        // Small enough that the chunk (comfortably over 60 bytes once encoded) cannot fit in one
        // `write`, so the first `write_all` call is guaranteed to still be waiting for room when
        // the timeout below fires.
        let (mut a, mut b) = tokio::io::duplex(16);

        assert!(
            timeout(
                Duration::from_millis(20),
                write_message(&mut a, &big, channel::VIDEO_BASE)
            )
            .await
            .is_err(),
            "the write must not have finished — otherwise this test proves nothing"
        );

        // An unrelated second message, sent the only way a caller without `ChunkWriter` can:
        // fresh, with no idea part of the previous chunk already leaked onto the wire.
        let ping = Message::Ping(Ping {
            seq: 99,
            sent_us: 1,
        });
        let writer_task = tokio::spawn(async move {
            write_message(&mut a, &ping, channel::CONTROL)
                .await
                .unwrap();
            // `a` drops here, closing the write half so the stuck read below gets an EOF
            // instead of hanging forever.
        });

        let mut reassembler = Reassembler::new();
        let result = timeout(
            Duration::from_secs(1),
            read_message(&mut b, &mut reassembler),
        )
        .await
        .expect("must not hang — the writer side closes once it is done");
        writer_task.await.unwrap();

        // The correct outcome — the peer cleanly seeing `ping` — must not happen. What actually
        // happens is the reader stalls waiting for the rest of a payload that was declared much
        // larger than what will ever arrive, and gets an `UnexpectedEof` once the writer closes.
        assert!(
            !matches!(result, Ok(Some(Message::Ping(_)))),
            "ping must not decode cleanly after the cancelled write — got {result:?}"
        );
    }

    /// The fix: with [`ChunkWriter`], the same cancellation-then-resume sequence that corrupts
    /// the stream above delivers both messages byte-exact and in order, because queueing is
    /// separate from flushing and flushing itself is cancel-safe.
    #[tokio::test]
    async fn chunk_writer_resumes_a_cancelled_flush_without_corrupting_the_stream() {
        use std::time::Duration;
        use tokio::time::timeout;

        let big = one_chunk_message();
        let (mut a, mut b) = tokio::io::duplex(16);

        let mut writer = ChunkWriter::new();
        writer.queue_message(&big, channel::VIDEO_BASE).unwrap();

        assert!(
            timeout(Duration::from_millis(20), writer.flush(&mut a))
                .await
                .is_err(),
            "must not finish in one shot — otherwise resuming below proves nothing"
        );
        assert!(
            writer.has_pending(),
            "the cancelled flush must leave the unsent tail queued, not discard it"
        );

        // Queue a second, unrelated message while the first is still only partially flushed —
        // exactly the situation that corrupted the stream in the `write_all`-based test above.
        let ping = Message::Ping(Ping {
            seq: 99,
            sent_us: 1,
        });
        writer.queue_message(&ping, channel::CONTROL).unwrap();

        let writer_task = tokio::spawn(async move {
            // Resuming is calling `flush` again, not re-queueing — it picks up exactly where it
            // stopped and then drains the freshly queued `ping` right after.
            writer.flush(&mut a).await.unwrap();
        });

        let mut reassembler = Reassembler::new();
        let got_big = timeout(
            Duration::from_secs(1),
            read_message(&mut b, &mut reassembler),
        )
        .await
        .expect("must not hang")
        .expect("io ok")
        .expect("known type");
        assert_eq!(got_big, big);

        let got_ping = timeout(
            Duration::from_secs(1),
            read_message(&mut b, &mut reassembler),
        )
        .await
        .expect("must not hang")
        .expect("io ok")
        .expect("known type");
        assert_eq!(got_ping, ping);

        writer_task.await.unwrap();
    }
}
