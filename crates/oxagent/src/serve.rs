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
use oxproto::message::input::key_flag;
use oxproto::message::window::window_flag;
use oxproto::message::{
    FrameData, Message, WindowClosed, WindowGeometry, WindowOpened, WindowTitle,
};
use oxproto::{feature, msg_type};
use oxtransport::{read_message, write_raw};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::handshake::{negotiate, HandshakeError, Negotiated};
use crate::input::InputSink;
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
    /// Whether the window is currently minimized.
    pub minimized: bool,
    /// Whether the window is currently maximized.
    pub maximized: bool,
    /// Whether the user can resize the window.
    pub resizable: bool,
    /// Whether the window has a plain, croppable native frame — see
    /// `crate::win::enumerate::has_native_frame` on the Windows side for exactly what this
    /// does and does not mean (it is not simply "has a title bar").
    pub has_frame: bool,
    /// Whether the window is marked always-on-top.
    pub topmost: bool,
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
pub async fn run_session<S, W, I>(
    stream: S,
    source: &mut W,
    sink: &mut I,
    params: SessionParams,
    session_id: u64,
    expected_token: &str,
) -> Result<Negotiated, HandshakeError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    W: WindowSource,
    I: InputSink,
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
    // TextInput and WindowControl are optional features (OXPROTO.md §8, §13): a client that
    // never negotiated them can still send the message (unknown-but-well-formed is not a
    // protocol error), it just should not silently work.
    let text_input_enabled = negotiated.features & feature::TEXT_INPUT != 0;
    let window_control_enabled = negotiated.features & feature::WINDOW_CONTROL != 0;

    let outcome = drive(
        &mut writer,
        source,
        sink,
        &mut rx,
        &mut ticker,
        &mut registry,
        &mut streams,
        &params,
        started,
        acks_enabled,
        text_input_enabled,
        window_control_enabled,
    )
    .await;

    reader_task.abort();
    let _ = writer.shutdown().await;
    outcome?;
    Ok(negotiated)
}

/// The steady-state loop: announce window changes, send frames, apply acks and input.
#[allow(clippy::too_many_arguments)]
async fn drive<W, WR, I>(
    writer: &mut WR,
    source: &mut W,
    sink: &mut I,
    rx: &mut mpsc::Receiver<Message>,
    ticker: &mut tokio::time::Interval,
    registry: &mut WindowRegistry,
    streams: &mut HashMap<u32, WindowStream>,
    params: &SessionParams,
    started: Instant,
    acks_enabled: bool,
    text_input_enabled: bool,
    window_control_enabled: bool,
) -> io::Result<()>
where
    W: WindowSource,
    WR: AsyncWrite + Unpin,
    I: InputSink,
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
                    Message::PointerEvent(p) => {
                        // `streams` is exactly the set of windows this session has announced
                        // and not yet closed, keyed by the same `window_id` the wire uses — so
                        // a miss here is "unknown or already-gone window", dropped rather than
                        // injected into whatever window happens to have focus.
                        if let Some(stream) = streams.get(&p.window_id) {
                            sink.pointer_event(
                                stream.handle,
                                p.x,
                                p.y,
                                p.buttons,
                                p.wheel_x,
                                p.wheel_y,
                            );
                        }
                    }
                    Message::KeyEvent(k) => {
                        // No `window_id`: keyboard input always targets whatever window is
                        // currently focused, which the sink establishes via `PointerEvent` /
                        // `WindowControl::ACTIVATE` (OXPROTO.md §13).
                        sink.key_event(k.scancode, k.flags & key_flag::EXTENDED != 0, k.is_pressed());
                    }
                    Message::TextInput(t) => {
                        if text_input_enabled {
                            sink.text_input(&t.text);
                        }
                    }
                    Message::ModifierSync(m) => {
                        sink.modifier_sync(m.modifiers, m.locks);
                    }
                    // Gated on WINDOW_CONTROL by the match guard: a client that never
                    // negotiated it falls through to the catch-all below and is dropped.
                    Message::WindowControl(w) if window_control_enabled => {
                        if let Some(stream) = streams.get(&w.window_id) {
                            sink.window_control(
                                stream.handle,
                                w.action,
                                w.x,
                                w.y,
                                w.width,
                                w.height,
                            );
                        }
                    }
                    // Anything else — handshake types replayed after negotiation, agent-to-
                    // client-only types echoed back — is accepted and ignored (protocol rule
                    // 6): dropping it is better than tearing the session down.
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

/// Pack a [`SourceWindow`]'s booleans into a `WindowOpened.flags` bitmask
/// (`OXPROTO.md` §11, `oxproto::message::window::window_flag`).
fn window_flags(w: &SourceWindow) -> u32 {
    let mut flags = 0;
    if w.resizable {
        flags |= window_flag::RESIZABLE;
    }
    if w.has_frame {
        flags |= window_flag::HAS_FRAME;
    }
    if w.topmost {
        flags |= window_flag::TOPMOST;
    }
    if w.minimized {
        flags |= window_flag::MINIMIZED;
    }
    if w.maximized {
        flags |= window_flag::MAXIMIZED;
    }
    flags
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
                    flags: window_flags(w),
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
                    minimized: false,
                    maximized: false,
                    resizable: true,
                    has_frame: true,
                    topmost: false,
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

    /// An [`InputSink`] that does nothing — for tests exercising everything except input.
    struct NoopSink;
    impl InputSink for NoopSink {
        fn pointer_event(&mut self, _: isize, _: i32, _: i32, _: u8, _: i16, _: i16) {}
        fn key_event(&mut self, _: u16, _: bool, _: bool) {}
        fn text_input(&mut self, _: &str) {}
        fn modifier_sync(&mut self, _: u16, _: u8) {}
        fn window_control(&mut self, _: isize, _: u8, _: i32, _: i32, _: u16, _: u16) {}
    }

    /// One recorded call to an [`InputSink`] method, for asserting exactly what the driver
    /// forwarded and with which arguments.
    #[derive(Debug, Clone, PartialEq)]
    enum SinkCall {
        Pointer {
            handle: isize,
            x: i32,
            y: i32,
            buttons: u8,
            wheel_x: i16,
            wheel_y: i16,
        },
        Key {
            scancode: u16,
            extended: bool,
            pressed: bool,
        },
        Text(String),
        ModifierSync {
            modifiers: u16,
            locks: u8,
        },
        WindowControl {
            handle: isize,
            action: u8,
            x: i32,
            y: i32,
            width: u16,
            height: u16,
        },
    }

    /// An [`InputSink`] that reports every call over a channel, so a test can assert on calls
    /// as they arrive without reaching into the sink after the session task has moved it.
    struct RecordingSink(mpsc::UnboundedSender<SinkCall>);

    impl InputSink for RecordingSink {
        fn pointer_event(
            &mut self,
            handle: isize,
            x: i32,
            y: i32,
            buttons: u8,
            wheel_x: i16,
            wheel_y: i16,
        ) {
            let _ = self.0.send(SinkCall::Pointer {
                handle,
                x,
                y,
                buttons,
                wheel_x,
                wheel_y,
            });
        }

        fn key_event(&mut self, scancode: u16, extended: bool, pressed: bool) {
            let _ = self.0.send(SinkCall::Key {
                scancode,
                extended,
                pressed,
            });
        }

        fn text_input(&mut self, text: &str) {
            let _ = self.0.send(SinkCall::Text(text.to_string()));
        }

        fn modifier_sync(&mut self, modifiers: u16, locks: u8) {
            let _ = self.0.send(SinkCall::ModifierSync { modifiers, locks });
        }

        fn window_control(
            &mut self,
            handle: isize,
            action: u8,
            x: i32,
            y: i32,
            width: u16,
            height: u16,
        ) {
            let _ = self.0.send(SinkCall::WindowControl {
                handle,
                action,
                x,
                y,
                width,
                height,
            });
        }
    }

    /// A [`SourceWindow`] with every flag-bearing field `false`, for building variations.
    fn plain_window() -> SourceWindow {
        SourceWindow {
            handle: 1,
            pid: 1,
            app_id: "a.exe".into(),
            title: "t".into(),
            x: 0,
            y: 0,
            width: 1,
            height: 1,
            dpi: 96,
            minimized: false,
            maximized: false,
            resizable: false,
            has_frame: false,
            topmost: false,
        }
    }

    #[test]
    fn window_flags_packs_every_bit_correctly() {
        use oxproto::message::window::window_flag;

        assert_eq!(
            window_flags(&plain_window()),
            0,
            "nothing set, nothing packed"
        );

        assert_eq!(
            window_flags(&SourceWindow {
                resizable: true,
                ..plain_window()
            }),
            window_flag::RESIZABLE
        );
        assert_eq!(
            window_flags(&SourceWindow {
                has_frame: true,
                ..plain_window()
            }),
            window_flag::HAS_FRAME
        );
        assert_eq!(
            window_flags(&SourceWindow {
                topmost: true,
                ..plain_window()
            }),
            window_flag::TOPMOST
        );
        assert_eq!(
            window_flags(&SourceWindow {
                minimized: true,
                ..plain_window()
            }),
            window_flag::MINIMIZED
        );
        assert_eq!(
            window_flags(&SourceWindow {
                maximized: true,
                ..plain_window()
            }),
            window_flag::MAXIMIZED
        );

        assert_eq!(
            window_flags(&SourceWindow {
                resizable: true,
                has_frame: true,
                topmost: true,
                minimized: true,
                maximized: true,
                ..plain_window()
            }),
            window_flag::RESIZABLE
                | window_flag::HAS_FRAME
                | window_flag::TOPMOST
                | window_flag::MINIMIZED
                | window_flag::MAXIMIZED,
            "every bit set must survive independently"
        );
    }

    fn hello(token: &str) -> Message {
        hello_with_features(token, feature::FRAME_ACK)
    }

    /// Like [`hello`], but with a caller-chosen feature set — for tests where negotiating (or
    /// not negotiating) `TEXT_INPUT`/`WINDOW_CONTROL` is the point.
    fn hello_with_features(token: &str, features: u64) -> Message {
        Message::ClientHello(ClientHello {
            version_min: 1,
            version_max: 1,
            features,
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
        use oxproto::message::window::window_flag;

        let (mut client, agent) = tokio::io::duplex(256 * 1024);
        let mut source = FakeSource::one_window(3);

        let agent_task = tokio::spawn(async move {
            let mut source = FakeSource::one_window(3);
            let mut sink = NoopSink;
            run_session(
                agent,
                &mut source,
                &mut sink,
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
        // `FakeSource::one_window` reports resizable+framed, not topmost/minimized/maximized —
        // `window_flags` must reflect exactly that, not the old hardcoded `0`.
        assert_eq!(w.flags, window_flag::RESIZABLE | window_flag::HAS_FRAME);

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
            let mut sink = NoopSink;
            run_session(
                agent,
                &mut source,
                &mut sink,
                SessionParams::default(),
                1,
                "secret",
            )
            .await
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
            let mut sink = NoopSink;
            run_session(
                agent,
                &mut source,
                &mut sink,
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

    /// Read messages from `client` until a `WindowOpened` arrives, and return it — every
    /// input test needs a real `window_id` to target before it can prove anything about
    /// dispatch to that window.
    async fn wait_for_window_opened(
        client: &mut tokio::io::DuplexStream,
        r: &mut Reassembler,
    ) -> WindowOpened {
        loop {
            match read_message(client, r).await.unwrap().unwrap() {
                Message::WindowOpened(w) => return w,
                _ => continue,
            }
        }
    }

    #[tokio::test]
    async fn pointer_key_and_modifier_events_reach_the_sink() {
        use oxproto::message::input::{key_flag, lock_state, modifier, pointer_button};
        use oxproto::message::{KeyEvent, ModifierSync, PointerEvent};

        let (mut client, agent) = tokio::io::duplex(64 * 1024);
        let (tx, mut rx) = mpsc::unbounded_channel();

        let agent_task = tokio::spawn(async move {
            let mut source = FakeSource::one_window(0);
            let mut sink = RecordingSink(tx);
            run_session(
                agent,
                &mut source,
                &mut sink,
                SessionParams::default(),
                3,
                "secret",
            )
            .await
        });

        let mut r = Reassembler::new();
        write_message(&mut client, &hello("secret"), channel::CONTROL)
            .await
            .unwrap();
        let opened = wait_for_window_opened(&mut client, &mut r).await;

        write_message(
            &mut client,
            &Message::PointerEvent(PointerEvent {
                window_id: opened.window_id,
                x: 12,
                y: -3,
                buttons: pointer_button::LEFT | pointer_button::RIGHT,
                wheel_x: 0,
                wheel_y: -120,
                timestamp: 0,
            }),
            channel::INPUT,
        )
        .await
        .unwrap();
        write_message(
            &mut client,
            &Message::KeyEvent(KeyEvent {
                scancode: 0x1E,
                flags: key_flag::PRESSED,
                timestamp: 0,
            }),
            channel::INPUT,
        )
        .await
        .unwrap();
        write_message(
            &mut client,
            &Message::ModifierSync(ModifierSync {
                modifiers: modifier::SHIFT,
                locks: lock_state::CAPS,
            }),
            channel::INPUT,
        )
        .await
        .unwrap();

        // `FakeSource::one_window` uses handle `0x1000` — the sink must see that native handle,
        // not the wire `window_id`, since only the driver knows the mapping between them.
        assert_eq!(
            rx.recv().await.unwrap(),
            SinkCall::Pointer {
                handle: 0x1000,
                x: 12,
                y: -3,
                buttons: pointer_button::LEFT | pointer_button::RIGHT,
                wheel_x: 0,
                wheel_y: -120,
            }
        );
        assert_eq!(
            rx.recv().await.unwrap(),
            SinkCall::Key {
                scancode: 0x1E,
                extended: false,
                pressed: true,
            }
        );
        assert_eq!(
            rx.recv().await.unwrap(),
            SinkCall::ModifierSync {
                modifiers: modifier::SHIFT,
                locks: lock_state::CAPS,
            }
        );

        drop(client);
        let _ = agent_task.await;
    }

    #[tokio::test]
    async fn window_control_targets_the_resolved_handle_when_negotiated() {
        use oxproto::message::input::window_action;
        use oxproto::message::WindowControl;

        let (mut client, agent) = tokio::io::duplex(64 * 1024);
        let (tx, mut rx) = mpsc::unbounded_channel();

        let agent_task = tokio::spawn(async move {
            let mut source = FakeSource::one_window(0);
            let mut sink = RecordingSink(tx);
            run_session(
                agent,
                &mut source,
                &mut sink,
                SessionParams::default(),
                4,
                "secret",
            )
            .await
        });

        let mut r = Reassembler::new();
        write_message(
            &mut client,
            &hello_with_features("secret", feature::WINDOW_CONTROL),
            channel::CONTROL,
        )
        .await
        .unwrap();
        let opened = wait_for_window_opened(&mut client, &mut r).await;

        write_message(
            &mut client,
            &Message::WindowControl(WindowControl {
                window_id: opened.window_id,
                action: window_action::MAXIMIZE,
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            }),
            channel::INPUT,
        )
        .await
        .unwrap();

        assert_eq!(
            rx.recv().await.unwrap(),
            SinkCall::WindowControl {
                handle: 0x1000,
                action: window_action::MAXIMIZE,
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            }
        );

        drop(client);
        let _ = agent_task.await;
    }

    #[tokio::test]
    async fn text_input_is_forwarded_only_when_negotiated() {
        use oxproto::message::TextInput;

        /// Runs one session with `extra_features` negotiated, sends a `TextInput` followed by
        /// a `ModifierSync` sentinel, and returns the first sink call observed — whichever of
        /// the two actually got through.
        async fn first_call_after_text_input(extra_features: u64) -> SinkCall {
            let (mut client, agent) = tokio::io::duplex(64 * 1024);
            let (tx, mut rx) = mpsc::unbounded_channel();

            let agent_task = tokio::spawn(async move {
                let mut source = FakeSource::one_window(0);
                let mut sink = RecordingSink(tx);
                run_session(
                    agent,
                    &mut source,
                    &mut sink,
                    SessionParams::default(),
                    1,
                    "secret",
                )
                .await
            });

            let mut r = Reassembler::new();
            write_message(
                &mut client,
                &hello_with_features("secret", extra_features),
                channel::CONTROL,
            )
            .await
            .unwrap();
            let _ = wait_for_window_opened(&mut client, &mut r).await;

            write_message(
                &mut client,
                &Message::TextInput(TextInput { text: "hi".into() }),
                channel::INPUT,
            )
            .await
            .unwrap();
            write_message(
                &mut client,
                &Message::ModifierSync(oxproto::message::ModifierSync {
                    modifiers: 0,
                    locks: 0,
                }),
                channel::INPUT,
            )
            .await
            .unwrap();

            let first = rx.recv().await.expect("the sentinel is always forwarded");
            drop(client);
            let _ = agent_task.await;
            first
        }

        assert_eq!(
            first_call_after_text_input(feature::TEXT_INPUT).await,
            SinkCall::Text("hi".into()),
            "negotiated TEXT_INPUT: the text reaches the sink before the sentinel"
        );
        assert_eq!(
            first_call_after_text_input(0).await,
            SinkCall::ModifierSync {
                modifiers: 0,
                locks: 0
            },
            "without TEXT_INPUT: the text is dropped, so the sentinel arrives first"
        );
    }

    #[tokio::test]
    async fn input_for_an_unknown_window_id_is_dropped() {
        use oxproto::message::input::{modifier, window_action};
        use oxproto::message::{ModifierSync, PointerEvent, WindowControl};

        let (mut client, agent) = tokio::io::duplex(64 * 1024);
        let (tx, mut rx) = mpsc::unbounded_channel();

        let agent_task = tokio::spawn(async move {
            let mut source = FakeSource::one_window(0);
            let mut sink = RecordingSink(tx);
            run_session(
                agent,
                &mut source,
                &mut sink,
                SessionParams::default(),
                9,
                "secret",
            )
            .await
        });

        let mut r = Reassembler::new();
        write_message(
            &mut client,
            &hello_with_features("secret", feature::FRAME_ACK | feature::WINDOW_CONTROL),
            channel::CONTROL,
        )
        .await
        .unwrap();
        let opened = wait_for_window_opened(&mut client, &mut r).await;
        let bogus_id = opened.window_id + 1;

        write_message(
            &mut client,
            &Message::PointerEvent(PointerEvent {
                window_id: bogus_id,
                x: 1,
                y: 1,
                buttons: 0,
                wheel_x: 0,
                wheel_y: 0,
                timestamp: 0,
            }),
            channel::INPUT,
        )
        .await
        .unwrap();
        write_message(
            &mut client,
            &Message::WindowControl(WindowControl {
                window_id: bogus_id,
                action: window_action::ACTIVATE,
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            }),
            channel::INPUT,
        )
        .await
        .unwrap();
        // A sentinel the driver always forwards, proving it kept processing after the two
        // unresolvable ids rather than injecting them anywhere or getting stuck on them.
        write_message(
            &mut client,
            &Message::ModifierSync(ModifierSync {
                modifiers: modifier::SHIFT,
                locks: 0,
            }),
            channel::INPUT,
        )
        .await
        .unwrap();

        assert_eq!(
            rx.recv().await.unwrap(),
            SinkCall::ModifierSync {
                modifiers: modifier::SHIFT,
                locks: 0
            },
            "the bogus-window-id events must never reach the sink"
        );

        drop(client);
        let _ = agent_task.await;
    }
}
