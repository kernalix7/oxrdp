//! The client session: handshake, then a stream of [`ClientEvent`]s.
//!
//! Generic over any tokio stream so it works over plain TCP in tests, TLS in production, and
//! QUIC later without change.

use std::io;

use oxproto::envelope::{channel, Reassembler};
use oxproto::message::{ClientHello, DisplayLayout, Message, Ping, Pong};
use oxproto::{error_code, feature, MIN_SUPPORTED_VERSION, PROTOCOL_VERSION};
use oxtransport::{read_message, write_message, ChunkReader};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

/// What the display/render layer reacts to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientEvent {
    /// A remote window appeared; create a native window for it.
    WindowOpened(oxproto::message::WindowOpened),
    /// A window moved or resized on the guest.
    WindowGeometry(oxproto::message::WindowGeometry),
    /// A window's title changed.
    WindowTitle(oxproto::message::WindowTitle),
    /// A window's show state changed.
    WindowState(oxproto::message::WindowState),
    /// A window's stacking position changed.
    WindowZOrder(oxproto::message::WindowZOrder),
    /// A window's icon arrived.
    WindowIcon(oxproto::message::WindowIcon),
    /// A remote window closed; destroy its native window.
    WindowClosed(oxproto::message::WindowClosed),
    /// A frame for a window; decode and present it.
    Frame(oxproto::message::FrameData),
    /// The cursor bitmap changed.
    CursorShape(oxproto::message::CursorShape),
    /// The cursor moved.
    CursorPosition(oxproto::message::CursorPosition),
    /// The cursor was shown or hidden.
    CursorVisibility(oxproto::message::CursorVisibility),
    /// The agent reported an error.
    Error(oxproto::message::Error),
    /// The agent is closing the session.
    Closed(oxproto::message::Close),
}

/// Configuration for [`ClientSession::connect`].
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// Shared secret provisioned out of band; the agent rejects anything else.
    pub auth_token: String,
    /// Name reported in the agent's logs.
    pub client_name: String,
    /// Codec ids in descending preference.
    pub codecs: Vec<u8>,
    /// The client's outputs, so the agent can match DPI and geometry.
    pub display: DisplayLayout,
}

/// A live session with the agent.
#[derive(Debug)]
pub struct ClientSession<S> {
    stream: S,
    reassembler: Reassembler,
    /// Read progress, kept here rather than in the read future so that cancelling
    /// [`ClientSession::next_event`] does not lose a partially-read chunk.
    chunks: ChunkReader,
    /// Bytes encoded but not yet handed to the stream, and how many of them have gone out.
    ///
    /// Every write goes through this buffer for two reasons. It makes writing resumable, for the
    /// same reason reads are; and it keeps writes ordered, so a pong that was interrupted
    /// half-written can never end up with another message spliced into the middle of it.
    pending_out: Vec<u8>,
    pending_written: usize,
    /// Protocol version both peers agreed on.
    pub version: u16,
    /// Features both peers advertised.
    pub features: u64,
    /// Codec the agent selected.
    pub codec: u8,
    /// Opaque session id, for correlating logs with the agent.
    pub session_id: u64,
}

impl<S: AsyncRead + AsyncWrite + Unpin> ClientSession<S> {
    /// Perform the handshake over an already-connected (and, in production, already
    /// encrypted) stream.
    pub async fn connect(mut stream: S, config: &SessionConfig) -> io::Result<Self> {
        let hello = Message::ClientHello(ClientHello {
            version_min: MIN_SUPPORTED_VERSION,
            version_max: PROTOCOL_VERSION,
            features: feature::SUPPORTED,
            auth_token: config.auth_token.clone(),
            client_name: config.client_name.clone(),
            codecs: config.codecs.clone(),
            display: config.display.clone(),
        });
        write_message(&mut stream, &hello, channel::CONTROL).await?;

        let mut reassembler = Reassembler::new();
        loop {
            match read_message(&mut stream, &mut reassembler).await? {
                Some(Message::ServerHello(sh)) => {
                    return Ok(Self {
                        stream,
                        reassembler,
                        chunks: ChunkReader::new(),
                        pending_out: Vec::new(),
                        pending_written: 0,
                        version: sh.version,
                        features: sh.features & feature::SUPPORTED,
                        codec: sh.codec,
                        session_id: sh.session_id,
                    })
                }
                Some(Message::Error(e)) => {
                    let what = if e.code == error_code::AUTH_FAILED {
                        "authentication rejected by the agent"
                    } else {
                        "agent refused the session"
                    };
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!("{what}: {} (code {})", e.message, e.code),
                    ));
                }
                // Anything else before ServerHello is a protocol violation.
                Some(other) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("expected ServerHello, got {:#04x}", other.msg_type()),
                    ))
                }
                // Unknown type: skip it, per the forward-compatibility rule.
                None => continue,
            }
        }
    }

    /// Whether a negotiated feature is active for this session.
    pub fn has_feature(&self, bit: u64) -> bool {
        self.features & bit != 0
    }

    /// Read the next event, answering protocol housekeeping (ping/pong) transparently.
    ///
    /// Returns `Ok(None)` when the peer closes the connection.
    ///
    /// # Cancellation
    ///
    /// Cancel-safe: all read and write progress is held in `self`, so dropping this future
    /// part-way through a chunk loses nothing and the next call resumes where it stopped. This
    /// is what makes it usable as a `tokio::select!` branch — the windowed client selects
    /// between this and the display's input events. An earlier version read through
    /// [`read_reassembled`], whose progress lived in the future: cancelling it discarded the
    /// bytes already consumed, the next read began mid-chunk, and the session died with a bogus
    /// "chunk payload exceeds MAX_CHUNK_PAYLOAD" once a payload byte was mistaken for a length.
    pub async fn next_event(&mut self) -> io::Result<Option<ClientEvent>> {
        loop {
            // Drain anything queued (a pong from a previous iteration) before blocking on the
            // read, so housekeeping cannot sit in the buffer while the peer waits for it.
            self.flush_pending().await?;

            let raw = match self
                .chunks
                .next_message(&mut self.stream, &mut self.reassembler)
                .await
            {
                Ok(raw) => raw,
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
                Err(e) => return Err(e),
            };
            let msg = match Message::decode_known(raw.msg_type, &raw.payload)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?
            {
                Some(msg) => msg,
                // An unimplemented message type is skipped rather than fatal.
                None => continue,
            };

            let event = match msg {
                Message::Ping(p) => {
                    let pong = Message::Pong(Pong {
                        seq: p.seq,
                        sent_us: p.sent_us,
                        // The agent owns the clock; the client echoes rather than inventing.
                        agent_us: 0,
                    });
                    // Queue rather than write: the next iteration flushes it, resumably.
                    self.queue(&pong)?;
                    continue;
                }
                Message::Pong(_) => continue,
                Message::WindowOpened(m) => ClientEvent::WindowOpened(m),
                Message::WindowGeometry(m) => ClientEvent::WindowGeometry(m),
                Message::WindowTitle(m) => ClientEvent::WindowTitle(m),
                Message::WindowState(m) => ClientEvent::WindowState(m),
                Message::WindowZOrder(m) => ClientEvent::WindowZOrder(m),
                Message::WindowIcon(m) => ClientEvent::WindowIcon(m),
                Message::WindowClosed(m) => ClientEvent::WindowClosed(m),
                Message::FrameData(m) => ClientEvent::Frame(m),
                Message::CursorShape(m) => ClientEvent::CursorShape(m),
                Message::CursorPosition(m) => ClientEvent::CursorPosition(m),
                Message::CursorVisibility(m) => ClientEvent::CursorVisibility(m),
                Message::Error(m) => ClientEvent::Error(m),
                Message::Close(m) => ClientEvent::Closed(m),
                // Client-to-agent messages arriving from the agent are a protocol violation,
                // but dropping them is friendlier than tearing the session down.
                _ => continue,
            };
            return Ok(Some(event));
        }
    }

    /// Send a message to the agent (input, acks, window control, quality hints).
    pub async fn send(&mut self, msg: &Message) -> io::Result<()> {
        self.queue(msg)?;
        self.flush_pending().await
    }

    /// Append a message's chunks to the outgoing buffer.
    fn queue(&mut self, msg: &Message) -> io::Result<()> {
        let chunks = msg
            .to_chunks(channel::CONTROL)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        for chunk in &chunks {
            self.pending_out.extend_from_slice(chunk);
        }
        Ok(())
    }

    /// Write out whatever is queued.
    ///
    /// Cancel-safe in the part that matters: `write` reports exactly how many bytes it took and
    /// consumes nothing when cancelled, and the count lives in `self`, so an interrupted write
    /// resumes mid-buffer instead of re-sending or losing bytes. The trailing `flush` only
    /// pushes bytes the stream has already accepted, so repeating it after a cancellation is
    /// harmless.
    async fn flush_pending(&mut self) -> io::Result<()> {
        while self.pending_written < self.pending_out.len() {
            let n = self
                .stream
                .write(&self.pending_out[self.pending_written..])
                .await?;
            if n == 0 {
                return Err(io::Error::from(io::ErrorKind::WriteZero));
            }
            self.pending_written += n;
        }
        self.pending_out.clear();
        self.pending_written = 0;
        self.stream.flush().await
    }

    /// Send a liveness probe.
    pub async fn ping(&mut self, seq: u32, sent_us: u64) -> io::Result<()> {
        self.send(&Message::Ping(Ping { seq, sent_us })).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxproto::message::{Error as ProtoError, FrameData, Output, ServerHello, WindowOpened};
    use oxtransport::write_message as srv_write;

    fn config() -> SessionConfig {
        SessionConfig {
            auth_token: "token".into(),
            client_name: "test".into(),
            codecs: vec![oxproto::codec::RAW_BGRA],
            display: DisplayLayout {
                outputs: vec![Output {
                    id: 0,
                    x: 0,
                    y: 0,
                    width: 800,
                    height: 600,
                    scale_num: 1,
                    scale_den: 1,
                    refresh_mhz: 60_000,
                }],
            },
        }
    }

    #[tokio::test]
    async fn handshake_then_events() {
        let (client_io, mut server_io) = tokio::io::duplex(64 * 1024);

        let server = tokio::spawn(async move {
            let mut r = Reassembler::new();
            // The client's hello arrives first.
            let hello = read_message(&mut server_io, &mut r).await.unwrap().unwrap();
            assert!(matches!(hello, Message::ClientHello(_)));

            srv_write(
                &mut server_io,
                &Message::ServerHello(ServerHello {
                    version: 1,
                    features: feature::CURSOR_STREAM | feature::FRAME_ACK,
                    session_id: 99,
                    codec: oxproto::codec::RAW_BGRA,
                }),
                channel::CONTROL,
            )
            .await
            .unwrap();

            // A ping must be answered by the session itself, not surfaced as an event.
            srv_write(
                &mut server_io,
                &Message::Ping(Ping { seq: 1, sent_us: 5 }),
                channel::CONTROL,
            )
            .await
            .unwrap();
            let pong = read_message(&mut server_io, &mut r).await.unwrap().unwrap();
            assert!(matches!(pong, Message::Pong(p) if p.seq == 1));

            srv_write(
                &mut server_io,
                &Message::WindowOpened(WindowOpened {
                    window_id: 1,
                    video_channel: channel::VIDEO_BASE,
                    pid: 4,
                    app_id: "notepad.exe".into(),
                    title: "Untitled".into(),
                    x: 0,
                    y: 0,
                    width: 800,
                    height: 600,
                    dpi: 96,
                    flags: 0,
                    owner_id: 0,
                }),
                channel::CONTROL,
            )
            .await
            .unwrap();

            srv_write(
                &mut server_io,
                &Message::FrameData(FrameData {
                    window_id: 1,
                    frame_id: 1,
                    codec: oxproto::codec::RAW_BGRA,
                    flags: 1,
                    width: 2,
                    height: 1,
                    captured_us: 1,
                    encoded_us: 2,
                    data: vec![1, 2, 3, 4, 5, 6, 7, 8],
                }),
                channel::VIDEO_BASE,
            )
            .await
            .unwrap();
        });

        let mut session = ClientSession::connect(client_io, &config()).await.unwrap();
        assert_eq!(session.version, 1);
        assert_eq!(session.session_id, 99);
        assert_eq!(session.codec, oxproto::codec::RAW_BGRA);
        assert!(session.has_feature(feature::FRAME_ACK));

        match session.next_event().await.unwrap().unwrap() {
            ClientEvent::WindowOpened(w) => {
                assert_eq!(w.app_id, "notepad.exe");
                assert_eq!(w.video_channel, channel::VIDEO_BASE);
            }
            other => panic!("expected WindowOpened, got {other:?}"),
        }
        match session.next_event().await.unwrap().unwrap() {
            ClientEvent::Frame(f) => {
                assert_eq!(f.window_id, 1);
                assert_eq!(f.data.len(), 8);
            }
            other => panic!("expected Frame, got {other:?}"),
        }

        server.await.unwrap();
    }

    #[tokio::test]
    async fn auth_failure_is_reported() {
        let (client_io, mut server_io) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            let mut r = Reassembler::new();
            let _ = read_message(&mut server_io, &mut r).await.unwrap();
            srv_write(
                &mut server_io,
                &Message::Error(ProtoError {
                    code: error_code::AUTH_FAILED,
                    message: "bad token".into(),
                }),
                channel::CONTROL,
            )
            .await
            .unwrap();
        });

        let err = ClientSession::connect(client_io, &config())
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("bad token"));
    }

    #[tokio::test]
    async fn closed_stream_ends_the_event_loop() {
        let (client_io, mut server_io) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            let mut r = Reassembler::new();
            let _ = read_message(&mut server_io, &mut r).await.unwrap();
            srv_write(
                &mut server_io,
                &Message::ServerHello(ServerHello {
                    version: 1,
                    features: 0,
                    session_id: 1,
                    codec: 1,
                }),
                channel::CONTROL,
            )
            .await
            .unwrap();
            // Drop the server side, closing the connection.
        });

        let mut session = ClientSession::connect(client_io, &config()).await.unwrap();
        assert_eq!(session.next_event().await.unwrap(), None);
    }
}
