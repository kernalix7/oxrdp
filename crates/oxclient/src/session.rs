//! The client session: handshake, then a stream of [`ClientEvent`]s.
//!
//! Generic over any tokio stream so it works over plain TCP in tests, TLS in production, and
//! QUIC later without change.

use std::io;

use oxproto::envelope::{channel, Reassembler};
use oxproto::latency::ClockSync;
use oxproto::message::{ClientHello, DisplayLayout, Message, Ping, Pong};
use oxproto::{error_code, feature, MIN_SUPPORTED_VERSION, PROTOCOL_VERSION};
use oxtransport::{read_message, write_message, ChunkReader};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use crate::clock::ClientClock;

/// Round-trip samples kept for the clock-offset estimate.
///
/// At one ping a second (`OXPROTO.md` §15) this is a couple of minutes of history — enough for
/// the minimum to represent a genuinely unqueued round trip, which is the one worth trusting.
const RTT_WINDOW: usize = 128;

/// Exchanges kept as candidates for the offset estimate.
const OFFSET_CANDIDATES: usize = 64;

/// Assumed worst-case relative drift between the two clocks, in parts per million.
///
/// Ordinary crystals are specified to tens of ppm and a virtualised guest clock is usually
/// disciplined on top of that, so 100 ppm is conservative rather than typical. It is an
/// assumption, and it is stated here rather than buried because the error bound depends on it:
/// at 100 ppm a sample one minute old has drifted 6 ms, which dwarfs half the round trip of a
/// fast exchange. An estimate that ignored this would quote a bound far tighter than it earns.
const CLOCK_DRIFT_PPM: u64 = 100;

/// One ping/pong exchange, kept as a candidate for the offset estimate.
#[derive(Debug, Clone, Copy)]
struct OffsetSample {
    rtt_us: u64,
    offset_us: i64,
    taken_at_us: u64,
}

impl OffsetSample {
    /// How far this sample's offset could be out if used at `now_us`.
    ///
    /// Two independent errors. Half the round trip is what the symmetric-path assumption costs
    /// when the path is not symmetric. The drift term is what the two clocks do to each other
    /// while the sample ages. Both are upper bounds, and they add.
    fn error_bound_us(&self, now_us: u64) -> u64 {
        let age_us = now_us.saturating_sub(self.taken_at_us);
        self.rtt_us / 2 + age_us.saturating_mul(CLOCK_DRIFT_PPM) / 1_000_000
    }
}

/// Which exchange the offset estimate should come from.
///
/// Not the most recent one. An offset is only as trustworthy as the symmetry of the round trip
/// it was measured over, so a single congested exchange poisons the estimate for as long as it
/// is the latest — which was observed: a 100 ms round trip produced a +/-50 ms bound on a stage
/// whose median was 7 ms, making the figure unusable.
///
/// Nor simply the lowest round trip ever seen, which is the standard NTP answer and is
/// incomplete here: the best exchange may be minutes old, and an old offset is wrong by however
/// far the clocks have drifted apart since. So the sample chosen is the one whose *total* error
/// bound is smallest — round-trip asymmetry plus accumulated drift — which prefers a fast recent
/// exchange, tolerates an older one when nothing better exists, and lets a stale sample age out
/// on its own rather than by a rule about how old is too old.
#[derive(Debug, Clone)]
struct OffsetEstimate {
    recent: std::collections::VecDeque<OffsetSample>,
}

impl OffsetEstimate {
    fn new() -> Self {
        Self {
            recent: std::collections::VecDeque::new(),
        }
    }

    fn push(&mut self, sample: OffsetSample) {
        if self.recent.len() >= OFFSET_CANDIDATES {
            self.recent.pop_front();
        }
        self.recent.push_back(sample);
    }

    /// The candidate with the smallest total error bound at `now_us`.
    fn best(&self, now_us: u64) -> Option<OffsetSample> {
        self.recent
            .iter()
            .copied()
            .min_by_key(|sample| sample.error_bound_us(now_us))
    }
}

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
    /// The session's monotonic clock. Every client-side timestamp on the wire comes from here,
    /// and so does everything the latency accounting compares, so they share one epoch.
    clock: ClientClock,
    /// Round-trip time and agent-clock offset, from the ping/pong exchange below.
    clock_sync: ClockSync,
    /// Candidate offsets, so the estimate can come from the best exchange rather than the last.
    offsets: OffsetEstimate,
    /// Sequence number for the next ping this client sends.
    ping_seq: u32,
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
                        clock: ClientClock::new(),
                        clock_sync: ClockSync::new(RTT_WINDOW),
                        offsets: OffsetEstimate::new(),
                        ping_seq: 0,
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
                // The agent's answer to one of our pings. It carries the agent's own clock, so
                // this is the only thing that lets an agent timestamp be compared with a client
                // one. Consumed here rather than surfaced: it is housekeeping, like the ping.
                Message::Pong(p) => {
                    let now_us = self.clock.now_us();
                    if let Some(rtt_us) = self.clock_sync.on_pong(p.seq, p.agent_us, now_us) {
                        if let Some(offset_us) = self.clock_sync.offset_us() {
                            self.offsets.push(OffsetSample {
                                rtt_us,
                                offset_us,
                                taken_at_us: now_us,
                            });
                        }
                    }
                    continue;
                }
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

    /// The session's clock. Copy it rather than starting another, or timestamps taken in
    /// different places cannot be compared.
    pub fn clock(&self) -> ClientClock {
        self.clock
    }

    /// How far ahead of the client's clock the agent's is, once a pong has landed.
    ///
    /// The estimate assumes the network delays both directions equally, so its error is half the
    /// path asymmetry and is bounded by half the round-trip time. Read it together with
    /// [`ClientSession::rtt_us`]: an offset from a 1 ms round trip is worth far more than one
    /// from a 40 ms round trip.
    pub fn clock_offset_us(&self) -> Option<i64> {
        Some(self.offsets.best(self.clock.now_us())?.offset_us)
    }

    /// How far the offset estimate could be out.
    ///
    /// The bound of the exchange the offset actually came from — round-trip asymmetry plus the
    /// drift accumulated since it was taken — never of some other, more flattering sample.
    pub fn offset_error_bound_us(&self) -> Option<u64> {
        let now_us = self.clock.now_us();
        Some(self.offsets.best(now_us)?.error_bound_us(now_us))
    }

    /// Round-trip time statistics from the ping/pong exchange.
    pub fn rtt_us(&self) -> &oxproto::latency::Samples {
        self.clock_sync.rtt()
    }

    /// Send a liveness probe, and register it so the matching pong updates the clock estimate.
    ///
    /// `OXPROTO.md` §15 has both ends do this every second. Until this was called the client
    /// sent no pings at all and discarded every pong, so there was no way to relate an agent
    /// timestamp to a client one.
    pub async fn ping(&mut self) -> io::Result<()> {
        let seq = self.ping_seq;
        self.ping_seq = self.ping_seq.wrapping_add(1);
        let sent_us = self.clock.now_us();
        self.clock_sync.on_ping_sent(seq, sent_us);
        self.send(&Message::Ping(Ping { seq, sent_us })).await
    }
}

#[cfg(test)]
mod offset_tests {
    use super::*;

    fn sample(rtt_us: u64, offset_us: i64, taken_at_us: u64) -> OffsetSample {
        OffsetSample {
            rtt_us,
            offset_us,
            taken_at_us,
        }
    }

    /// The defect this replaces: one congested exchange became the estimate simply by being
    /// last, and a 100 ms round trip put a +/-50 ms bound on a stage with a 7 ms median.
    #[test]
    fn a_congested_exchange_does_not_displace_a_good_one() {
        let mut estimate = OffsetEstimate::new();
        estimate.push(sample(1_000, 4_000, 1_000_000));
        estimate.push(sample(100_000, 9_999, 1_500_000));

        let best = estimate.best(2_000_000).expect("a sample exists");

        assert_eq!(best.offset_us, 4_000, "the fast exchange should win");
        // Half of 1 ms, plus 1 s of drift at 100 ppm.
        assert_eq!(best.error_bound_us(2_000_000), 500 + 100);
    }

    /// And the part plain lowest-RTT selection gets wrong: a very old best sample is not best
    /// any more, because the clocks have drifted apart since it was taken.
    #[test]
    fn a_stale_fast_sample_loses_to_a_fresh_slower_one() {
        let mut estimate = OffsetEstimate::new();
        // Excellent round trip, but taken 100 seconds ago: 10 ms of possible drift since.
        estimate.push(sample(200, -1_000, 0));
        // Ten times the round trip, but taken just now.
        estimate.push(sample(2_000, -1_200, 100_000_000));

        let best = estimate.best(100_000_000).expect("a sample exists");

        assert_eq!(
            best.offset_us, -1_200,
            "10 ms of drift beats 0.9 ms of round-trip asymmetry"
        );
        assert_eq!(best.error_bound_us(100_000_000), 1_000);
    }

    #[test]
    fn the_bound_is_the_selected_sample_s_own_and_grows_as_it_ages() {
        let one = sample(1_000, 0, 0);

        assert_eq!(one.error_bound_us(0), 500, "half the round trip, no age");
        assert_eq!(one.error_bound_us(10_000_000), 500 + 1_000, "10 s of drift");
        assert_eq!(one.error_bound_us(60_000_000), 500 + 6_000, "60 s of drift");
    }

    #[test]
    fn candidates_are_bounded_and_the_estimate_survives_eviction() {
        let mut estimate = OffsetEstimate::new();
        for i in 0..(OFFSET_CANDIDATES as u64 + 20) {
            estimate.push(sample(5_000, i as i64, i * 1_000_000));
        }

        assert_eq!(estimate.recent.len(), OFFSET_CANDIDATES);
        assert!(
            estimate.best(100_000_000).is_some(),
            "eviction must not empty the estimate"
        );
    }

    #[test]
    fn nothing_is_claimed_before_the_first_pong() {
        assert!(OffsetEstimate::new().best(1_000_000).is_none());
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
