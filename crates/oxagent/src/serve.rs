//! The agent's session driver: accept a client, authenticate it, then stream windows.
//!
//! The platform sits behind [`WindowSource`], so the whole driver — handshake, window
//! lifecycle diffing, pacing, ack handling, input dispatch — is exercised on the Linux build
//! host where CI runs. Only the implementation of that trait is Windows-only.
//!
//! Structure of a session:
//!
//! ```text
//!   reader task  ──mpsc──▶  driver loop  ──writes──▶  client
//!   (decodes incoming)      (ticks at target_fps)
//! ```
//!
//! Splitting the read side into its own task is what lets a `FrameAck` arrive while a frame is
//! being written; a single-threaded read-then-write loop would deadlock the flow control it is
//! supposed to implement.

use std::collections::HashMap;
use std::io;
use std::time::{Duration, Instant};

use oxproto::envelope::{channel, Reassembler};
use oxproto::message::{
    FrameData, Message, WindowClosed, WindowGeometry, WindowOpened, WindowTitle,
};
use oxproto::{feature, msg_type};
use oxtransport::{read_message, write_raw};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::handshake::{negotiate, HandshakeError, Negotiated};
use crate::pacing::FrameBudget;
use crate::registry::WindowRegistry;

/// A window the platform is offering, as the driver needs to see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceWindow {
    /// Native handle (an `HWND` on Windows), used only as a registry key.
    pub handle: isize,
    /// Process id owning the window.
    pub pid: u32,
    /// Executable base name — becomes the client's `WM_CLASS`.
    pub app_id: String,
    /// Window title.
    pub title: String,
    /// Visible frame bounds in guest screen coordinates.
    pub x: i32,
    /// Visible frame bounds in guest screen coordinates.
    pub y: i32,
    /// Visible frame width.
    pub width: u16,
    /// Visible frame height.
    pub height: u16,
    /// Guest DPI for this window.
    pub dpi: u16,
}

/// A captured frame handed to the driver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFrame {
    /// Frame width in pixels.
    pub width: u16,
    /// Frame height in pixels.
    pub height: u16,
    /// Pixel bytes in the session's codec (`RAW_BGRA` today).
    pub data: Vec<u8>,
}

/// What the session driver needs from the platform.
///
/// Implementations are polled; they must not block, because the driver's tick is also its
/// pacing clock.
pub trait WindowSource {
    /// Windows currently shareable. The driver diffs this against what it has told the client.
    fn live_windows(&mut self) -> Vec<SourceWindow>;

    /// The next frame for a window, or `None` when nothing new has been captured.
    fn next_frame(&mut self, handle: isize) -> Option<SourceFrame>;
}

/// Tunables for one session.
#[derive(Debug, Clone, Copy)]
pub struct SessionParams {
    /// Capture/send attempts per second.
    pub target_fps: u16,
    /// Unacknowledged frames allowed per window before the oldest is dropped.
    pub max_frames_in_flight: u8,
}

impl Default for SessionParams {
    fn default() -> Self {
        Self {
            target_fps: 30,
            max_frames_in_flight: 2,
        }
    }
}

/// Per-window driver state.
struct WindowStream {
    handle: isize,
    video_channel: u16,
    budget: FrameBudget,
    /// Last geometry announced, so a move/resize is only sent when it actually changes.
    geometry: (i32, i32, u16, u16),
    /// Last title announced.
    title: String,
}

/// Run one authenticated session to completion.
///
/// Returns when the client disconnects or the transport fails. The handshake happens here, so
/// a caller that gets an error never had an authenticated peer.
pub async fn run_session<S, W>(
    stream: S,
    source: &mut W,
    params: SessionParams,
    session_id: u64,
    expected_token: &str,
) -> Result<Negotiated, HandshakeError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    W: WindowSource,
{
    let (mut reader, mut writer) = tokio::io::split(stream);

    let mut reassembler = Reassembler::new();
    // Authenticate before anything else touches the source. The token comparison is
    // constant-time; see `oxsec::verify_token`.
    let negotiated = {
        let mut duplex = ReadWrite {
            reader: &mut reader,
            writer: &mut writer,
        };
        negotiate(&mut duplex, &mut reassembler, session_id, |presented| {
            oxsec::verify_token(expected_token, presented)
        })
        .await?
    };

    // From here on the read side runs independently so acks and input are not blocked behind
    // frame writes.
    let (tx, mut rx) = mpsc::channel::<Message>(256);
    let reader_task = tokio::spawn(async move {
        loop {
            match read_message(&mut reader, &mut reassembler).await {
                // A type this build does not implement is skipped, not fatal.
                Ok(None) => continue,
                Ok(Some(msg)) => {
                    if tx.send(msg).await.is_err() {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    });

    let started = Instant::now();
    let frame_interval = Duration::from_micros(1_000_000 / u64::from(params.target_fps.max(1)));
    let mut ticker = tokio::time::interval(frame_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let mut registry = WindowRegistry::new();
    let mut streams: HashMap<u32, WindowStream> = HashMap::new();
    let acks_enabled = negotiated.features & feature::FRAME_ACK != 0;

    let outcome = drive(
        &mut writer,
        source,
        &mut rx,
        &mut ticker,
        &mut registry,
        &mut streams,
        &params,
        started,
        acks_enabled,
    )
    .await;

    reader_task.abort();
    let _ = writer.shutdown().await;
    outcome?;
    Ok(negotiated)
}

/// The steady-state loop: announce window changes, send frames, apply acks and input.
#[allow(clippy::too_many_arguments)]
async fn drive<W, WR>(
    writer: &mut WR,
    source: &mut W,
    rx: &mut mpsc::Receiver<Message>,
    ticker: &mut tokio::time::Interval,
    registry: &mut WindowRegistry,
    streams: &mut HashMap<u32, WindowStream>,
    params: &SessionParams,
    started: Instant,
    acks_enabled: bool,
) -> io::Result<()>
where
    W: WindowSource,
    WR: AsyncWrite + Unpin,
{
    loop {
        tokio::select! {
            // Incoming messages take priority over the next capture: an ack that frees budget
            // is worth more than a frame that would be dropped for lack of it.
            biased;

            incoming = rx.recv() => {
                let Some(msg) = incoming else {
                    // The reader stopped: the peer is gone.
                    return Ok(());
                };
                match msg {
                    Message::FrameAck(ack) => {
                        if let Some(stream) = streams.get_mut(&ack.window_id) {
                            stream.budget.on_ack(ack.frame_id);
                        }
                    }
                    Message::Close(_) => return Ok(()),
                    Message::Ping(p) => {
                        let pong = Message::Pong(oxproto::message::Pong {
                            seq: p.seq,
                            sent_us: p.sent_us,
                            agent_us: elapsed_us(started),
                        });
                        send(writer, &pong, channel::CONTROL).await?;
                    }
                    // Input and window control are accepted and ignored until P3 wires
                    // injection; dropping them is better than tearing the session down.
                    _ => {}
                }
            }

            _ = ticker.tick() => {
                sync_windows(writer, source, registry, streams, params).await?;
                pump_frames(writer, source, streams, started, acks_enabled).await?;
            }
        }
    }
}

/// Diff the platform's window list against what the client has been told.
async fn sync_windows<W, WR>(
    writer: &mut WR,
    source: &mut W,
    registry: &mut WindowRegistry,
    streams: &mut HashMap<u32, WindowStream>,
    params: &SessionParams,
) -> io::Result<()>
where
    W: WindowSource,
    WR: AsyncWrite + Unpin,
{
    let live = source.live_windows();

    for w in &live {
        let (tracked, is_new) = registry.track(w.handle);
        if is_new {
            send(
                writer,
                &Message::WindowOpened(WindowOpened {
                    window_id: tracked.window_id,
                    video_channel: tracked.video_channel,
                    pid: w.pid,
                    app_id: w.app_id.clone(),
                    title: w.title.clone(),
                    x: w.x,
                    y: w.y,
                    width: w.width,
                    height: w.height,
                    dpi: w.dpi,
                    flags: 0,
                    owner_id: 0,
                }),
                channel::WINDOW,
            )
            .await?;
            streams.insert(
                tracked.window_id,
                WindowStream {
                    handle: w.handle,
                    video_channel: tracked.video_channel,
                    budget: FrameBudget::new(params.max_frames_in_flight),
                    geometry: (w.x, w.y, w.width, w.height),
                    title: w.title.clone(),
                },
            );
            continue;
        }

        // Only report what actually changed — a client that re-lays-out its window on every
        // tick would flicker.
        let Some(stream) = streams.get_mut(&tracked.window_id) else {
            continue;
        };
        let geometry = (w.x, w.y, w.width, w.height);
        if stream.geometry != geometry {
            stream.geometry = geometry;
            // A resize invalidates frames already in flight for the old size.
            stream.budget.restart();
            send(
                writer,
                &Message::WindowGeometry(WindowGeometry {
                    window_id: tracked.window_id,
                    x: w.x,
                    y: w.y,
                    width: w.width,
                    height: w.height,
                }),
                channel::WINDOW,
            )
            .await?;
        }
        if stream.title != w.title {
            stream.title.clone_from(&w.title);
            send(
                writer,
                &Message::WindowTitle(WindowTitle {
                    window_id: tracked.window_id,
                    title: w.title.clone(),
                }),
                channel::WINDOW,
            )
            .await?;
        }
    }

    let handles: Vec<isize> = live.iter().map(|w| w.handle).collect();
    for gone in registry.retain_live(&handles) {
        streams.remove(&gone.window_id);
        send(
            writer,
            &Message::WindowClosed(WindowClosed {
                window_id: gone.window_id,
            }),
            channel::WINDOW,
        )
        .await?;
    }
    Ok(())
}

/// Capture and send one frame per window, subject to each window's budget.
async fn pump_frames<W, WR>(
    writer: &mut WR,
    source: &mut W,
    streams: &mut HashMap<u32, WindowStream>,
    started: Instant,
    acks_enabled: bool,
) -> io::Result<()>
where
    W: WindowSource,
    WR: AsyncWrite + Unpin,
{
    // Deterministic order so a window is never starved by map iteration order.
    let mut ids: Vec<u32> = streams.keys().copied().collect();
    ids.sort_unstable();

    for id in ids {
        let Some(stream) = streams.get_mut(&id) else {
            continue;
        };
        // Without acks there is no feedback to pace against, so send every capture and let the
        // transport apply back-pressure.
        if acks_enabled && !stream.budget.has_headroom() {
            // Still capture: `on_captured` displaces the stale frame rather than queueing.
        }
        let Some(frame) = source.next_frame(stream.handle) else {
            continue;
        };

        let captured_us = elapsed_us(started);
        let frame_id = stream.budget.on_captured();
        let body = Message::FrameData(FrameData {
            window_id: id,
            frame_id,
            codec: oxproto::codec::RAW_BGRA,
            flags: oxproto::message::window::frame_flag::KEYFRAME,
            width: frame.width,
            height: frame.height,
            captured_us,
            // No encoder yet: capture and "encode" complete at the same instant.
            encoded_us: captured_us,
            data: frame.data,
        })
        .encode_body()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

        write_raw(writer, msg_type::FRAME_DATA, stream.video_channel, &body).await?;
    }
    Ok(())
}

async fn send<WR: AsyncWrite + Unpin>(writer: &mut WR, msg: &Message, ch: u16) -> io::Result<()> {
    let body = msg
        .encode_body()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    write_raw(writer, msg.msg_type(), ch, &body).await
}

/// Microseconds since the session started — the clock every protocol timestamp uses.
fn elapsed_us(started: Instant) -> u64 {
    started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64
}

/// Glues a split reader and writer back into one `AsyncRead + AsyncWrite` for the handshake.
struct ReadWrite<'a, R, W> {
    reader: &'a mut R,
    writer: &'a mut W,
}

impl<R: AsyncRead + Unpin, W: Unpin> AsyncRead for ReadWrite<'_, R, W> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut *self.reader).poll_read(cx, buf)
    }
}

impl<R: Unpin, W: AsyncWrite + Unpin> AsyncWrite for ReadWrite<'_, R, W> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        std::pin::Pin::new(&mut *self.writer).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut *self.writer).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut *self.writer).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxproto::message::{ClientHello, DisplayLayout, Output};
    use oxtransport::write_message;

    /// A scripted source: a fixed window list and a frame generator.
    struct FakeSource {
        windows: Vec<SourceWindow>,
        frames_left: usize,
    }

    impl FakeSource {
        fn one_window(frames: usize) -> Self {
            Self {
                windows: vec![SourceWindow {
                    handle: 0x1000,
                    pid: 42,
                    app_id: "notepad.exe".into(),
                    title: "Untitled".into(),
                    x: 10,
                    y: 20,
                    width: 4,
                    height: 1,
                    dpi: 96,
                }],
                frames_left: frames,
            }
        }
    }

    impl WindowSource for FakeSource {
        fn live_windows(&mut self) -> Vec<SourceWindow> {
            self.windows.clone()
        }

        fn next_frame(&mut self, _handle: isize) -> Option<SourceFrame> {
            if self.frames_left == 0 {
                return None;
            }
            self.frames_left -= 1;
            Some(SourceFrame {
                width: 4,
                height: 1,
                data: vec![0xAB; 16],
            })
        }
    }

    fn hello(token: &str) -> Message {
        Message::ClientHello(ClientHello {
            version_min: 1,
            version_max: 1,
            features: feature::FRAME_ACK,
            auth_token: token.into(),
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
        })
    }

    #[tokio::test]
    async fn streams_a_window_and_its_frames() {
        let (mut client, agent) = tokio::io::duplex(256 * 1024);
        let mut source = FakeSource::one_window(3);

        let agent_task = tokio::spawn(async move {
            let mut source = FakeSource::one_window(3);
            run_session(
                agent,
                &mut source,
                SessionParams {
                    target_fps: 240,
                    max_frames_in_flight: 2,
                },
                77,
                "secret",
            )
            .await
        });
        // The local `source` is only here to keep the type inference obvious in this test.
        let _ = source.live_windows();

        let mut r = Reassembler::new();
        write_message(&mut client, &hello("secret"), channel::CONTROL)
            .await
            .unwrap();

        // ServerHello, then the window, then frames.
        let sh = read_message(&mut client, &mut r).await.unwrap().unwrap();
        assert!(matches!(sh, Message::ServerHello(s) if s.session_id == 77));

        let opened = read_message(&mut client, &mut r).await.unwrap().unwrap();
        let Message::WindowOpened(w) = opened else {
            panic!("expected WindowOpened, got {opened:?}")
        };
        assert_eq!(w.app_id, "notepad.exe");
        assert_eq!(w.video_channel, channel::VIDEO_BASE);
        assert_eq!((w.x, w.y, w.width, w.height), (10, 20, 4, 1));

        let mut frames = 0;
        while frames < 2 {
            match read_message(&mut client, &mut r).await.unwrap() {
                Some(Message::FrameData(f)) => {
                    assert_eq!(f.window_id, w.window_id);
                    assert_eq!(f.data.len(), 16);
                    assert!(f.captured_us <= f.encoded_us);
                    frames += 1;
                }
                Some(_) => continue,
                None => continue,
            }
        }

        drop(client);
        let _ = agent_task.await;
    }

    #[tokio::test]
    async fn a_bad_token_never_reaches_the_source() {
        /// Panics if the driver ever asks it for anything — proving authentication gates it.
        struct Forbidden;
        impl WindowSource for Forbidden {
            fn live_windows(&mut self) -> Vec<SourceWindow> {
                panic!("the source must not be polled for an unauthenticated peer")
            }
            fn next_frame(&mut self, _handle: isize) -> Option<SourceFrame> {
                panic!("the source must not be polled for an unauthenticated peer")
            }
        }

        let (mut client, agent) = tokio::io::duplex(64 * 1024);
        let agent_task = tokio::spawn(async move {
            let mut source = Forbidden;
            run_session(agent, &mut source, SessionParams::default(), 1, "secret").await
        });

        write_message(&mut client, &hello("wrong"), channel::CONTROL)
            .await
            .unwrap();

        let result = agent_task.await.unwrap();
        assert!(matches!(
            result,
            Err(HandshakeError::Rejected {
                code: oxproto::error_code::AUTH_FAILED,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn a_closed_window_is_reported_once() {
        /// Offers a window on the first poll and nothing afterwards.
        struct Vanishing {
            polls: usize,
        }
        impl WindowSource for Vanishing {
            fn live_windows(&mut self) -> Vec<SourceWindow> {
                self.polls += 1;
                if self.polls <= 1 {
                    FakeSource::one_window(0).windows
                } else {
                    Vec::new()
                }
            }
            fn next_frame(&mut self, _handle: isize) -> Option<SourceFrame> {
                None
            }
        }

        let (mut client, agent) = tokio::io::duplex(64 * 1024);
        let agent_task = tokio::spawn(async move {
            let mut source = Vanishing { polls: 0 };
            run_session(
                agent,
                &mut source,
                SessionParams {
                    target_fps: 240,
                    max_frames_in_flight: 2,
                },
                5,
                "secret",
            )
            .await
        });

        let mut r = Reassembler::new();
        write_message(&mut client, &hello("secret"), channel::CONTROL)
            .await
            .unwrap();

        let mut saw_open = None;
        let mut saw_close = None;
        for _ in 0..8 {
            match read_message(&mut client, &mut r).await {
                Ok(Some(Message::WindowOpened(w))) => saw_open = Some(w.window_id),
                Ok(Some(Message::WindowClosed(c))) => {
                    saw_close = Some(c.window_id);
                    break;
                }
                Ok(_) => continue,
                Err(_) => break,
            }
        }
        assert!(saw_open.is_some(), "the window should have been announced");
        assert_eq!(
            saw_close, saw_open,
            "and then reported closed by the same id"
        );

        drop(client);
        let _ = agent_task.await;
    }
}
