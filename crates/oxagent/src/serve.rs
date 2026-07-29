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
use oxproto::message::window::{frame_flag, window_flag, window_show};
use oxproto::message::{
    Close, Error as ProtoError, FrameData, Message, WindowClosed, WindowGeometry, WindowOpened,
    WindowState, WindowTitle,
};
use oxproto::{close_reason, error_code, feature, msg_type};
use oxtransport::{read_message, write_message, write_raw};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, Semaphore};

use crate::encode::FrameEncoder;
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
    /// Last geometry announced, so a move/resize is only sent when it actually changes. Not
    /// updated while the window is minimized — see `sync_windows`.
    geometry: (i32, i32, u16, u16),
    /// Last title announced.
    title: String,
    /// Last show state announced (`window_show` value: `NORMAL`/`MINIMIZED`/`MAXIMIZED`).
    /// Doubles as the reason `pump_frames` stops capturing this window: while it reads
    /// `MINIMIZED`, `next_frame` is never called for this window's handle at all.
    show_state: u8,
    /// Last `window_flag` bitmask announced — the same set `WindowOpened.flags` carried at
    /// open, now tracked separately so a later change to any bit can be diffed and reported via
    /// `WindowState.flags` (`OXPROTO.md` §11).
    flags: u32,
    /// Whether the next frame sent for this window, once a codec that has a concept of one is
    /// in use, must be a keyframe. Starts `true` (a window's first frame in a session must
    /// always be one — `OXPROTO.md` §9.1 — including for a window that was already open when
    /// this client attached) and is set again whenever the coded size changes; cleared once a
    /// keyframe is actually sent. Meaningless, and untouched, for `RAW_BGRA`, which has no such
    /// concept.
    needs_keyframe: bool,
    /// How many `tick_to_capture_us` samples have been logged for this window so far, counted
    /// only up to `CAPTURE_DIAGNOSTIC_FRAME_LIMIT` — see `pump_frames`.
    capture_diag_logged: u32,
}

/// How many `tick_to_capture_us` samples, per window, to log — the same bounded,
/// permanent-diagnostic shape as `crate::win::encode`'s `DIAGNOSTIC_FRAME_LIMIT`, kept as its
/// own constant rather than shared: this one lives on the platform-independent side and measures
/// scheduling delay within `pump_frames` itself, not anything about the encoder.
const CAPTURE_DIAGNOSTIC_FRAME_LIMIT: u32 = 100;

/// Tracks which kinds of input this session has already logged the first occurrence of.
///
/// A pointer stream alone can run at 30+ Hz, so per-event logging would flood stderr and hide
/// the one thing this exists to answer: did input of this kind reach the sink at all, this
/// session. One line per kind, the first time it is actually dispatched, is enough to answer
/// that from the guest's own captured stderr without instrumenting anything else.
#[derive(Debug, Default)]
struct InputFirstSeen {
    pointer: bool,
    key: bool,
    text: bool,
    modifier_sync: bool,
    window_control: bool,
}

/// Run one authenticated session to completion.
///
/// Returns when the client disconnects or the transport fails. The handshake happens here, so
/// a caller that gets an error never had an authenticated peer.
///
/// `pre_auth_deadline` bounds the *whole* pre-authentication phase — reading `ClientHello`
/// here, plus whatever the caller spent accepting TLS before this was even called — as one
/// combined deadline against the same absolute instant, not two independent timeouts a slow
/// trickle could exploit at the boundary between them. `session_slot` enforces that exactly one
/// authenticated session drives windows at a time: connections are now handled concurrently
/// (each on its own task, so one stalled peer cannot block every connection after it — see
/// `crate::win::run_agent`), and without this gate that would let a second, fully authenticated
/// client also start streaming and injecting input alongside the first. `supported_codecs` is
/// forwarded to `negotiate` as-is — see there for why it is a parameter rather than a constant.
#[allow(clippy::too_many_arguments)]
pub async fn run_session<S, W, I, E>(
    stream: S,
    source: &mut W,
    sink: &mut I,
    encoder: &mut E,
    params: SessionParams,
    session_id: u64,
    expected_token: &str,
    supported_codecs: &[u8],
    pre_auth_deadline: Instant,
    session_slot: &Semaphore,
) -> Result<Negotiated, HandshakeError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    W: WindowSource,
    I: InputSink,
    E: FrameEncoder,
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
        match tokio::time::timeout_at(
            tokio::time::Instant::from(pre_auth_deadline),
            negotiate(
                &mut duplex,
                &mut reassembler,
                session_id,
                supported_codecs,
                |presented| oxsec::verify_token(expected_token, presented),
            ),
        )
        .await
        {
            Ok(result) => result?,
            // The peer is simply dropped, nothing is written back — deliberately, not because
            // `close_reason::IDLE_TIMEOUT` doesn't fit (it does, precisely). A well-behaved
            // client that stalled would appreciate knowing why; a peer holding the connection
            // open on purpose (a TCP zero-window stall is the textbook version) could just as
            // easily make the write itself hang, which would spend more of exactly the time
            // this deadline exists to stop spending. `write_message` here has no timeout of its
            // own to bound that risk, so silence is the safer default until one is added.
            Err(_elapsed) => return Err(HandshakeError::Timeout),
        }
    };

    // Exactly one authenticated session at a time. A second one is turned away *here* — after
    // it authenticated, so it gets a real, documented `Error` instead of a connection that just
    // hangs or silently drops — but before it can touch `source`/`sink` at all. `try_acquire`
    // rather than `acquire`: a client that cannot get the slot right now must be told so, not
    // queued to start streaming later once the first session happens to end.
    let Ok(_session_permit) = session_slot.try_acquire() else {
        let _ = write_message(
            &mut writer,
            &Message::Error(ProtoError {
                code: error_code::SESSION_BUSY,
                message: "a session is already active".into(),
            }),
            channel::CONTROL,
        )
        .await;
        // The same reason every other post-`Error` `Close` in this crate uses (see
        // `crate::handshake::reject`).
        let _ = write_message(
            &mut writer,
            &Message::Close(Close {
                reason: close_reason::ERROR,
            }),
            channel::CONTROL,
        )
        .await;
        return Err(HandshakeError::Busy);
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
        encoder,
        &mut rx,
        &mut ticker,
        &mut registry,
        &mut streams,
        &params,
        started,
        acks_enabled,
        text_input_enabled,
        window_control_enabled,
        negotiated.codec,
    )
    .await;

    reader_task.abort();
    let _ = writer.shutdown().await;
    outcome?;
    Ok(negotiated)
}

/// The steady-state loop: announce window changes, send frames, apply acks and input.
#[allow(clippy::too_many_arguments)]
async fn drive<W, WR, I, E>(
    writer: &mut WR,
    source: &mut W,
    sink: &mut I,
    encoder: &mut E,
    rx: &mut mpsc::Receiver<Message>,
    ticker: &mut tokio::time::Interval,
    registry: &mut WindowRegistry,
    streams: &mut HashMap<u32, WindowStream>,
    params: &SessionParams,
    started: Instant,
    acks_enabled: bool,
    text_input_enabled: bool,
    window_control_enabled: bool,
    codec: u8,
) -> io::Result<()>
where
    W: WindowSource,
    WR: AsyncWrite + Unpin,
    I: InputSink,
    E: FrameEncoder,
{
    // See `InputFirstSeen`: logs the first dispatched event of each kind, and nothing else about
    // steady-state input traffic.
    let mut input_seen = InputFirstSeen::default();

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
                        //
                        // Accepted, not fixed: `streams` (via `sync_windows`) only re-resolves
                        // `window_id -> handle` once per tick, not on every message, so between
                        // two ticks a `handle` can go stale — the window closed and Windows
                        // recycled the `HWND` for a different, not-yet-announced window — and an
                        // event for the old id resolves to that new window until the next tick's
                        // diff notices the old handle is gone and reports it closed. This was
                        // reviewed and rated low: the guest is single-tenant and the one client
                        // this session has can already drive whatever window is focused, so
                        // hitting the recycled handle grants no capability it did not already
                        // have. Do not add re-resolution machinery for this — see the security
                        // review.
                        if let Some(stream) = streams.get(&p.window_id) {
                            if !input_seen.pointer {
                                input_seen.pointer = true;
                                eprintln!(
                                    "oxagent: input: first PointerEvent this session: window_id={} handle={:#x} buttons={:#04x}",
                                    p.window_id, stream.handle, p.buttons
                                );
                            }
                            sink.pointer_event(
                                stream.handle,
                                p.x,
                                p.y,
                                p.buttons,
                                p.wheel_x,
                                p.wheel_y,
                            );
                        } else {
                            eprintln!(
                                "oxagent: input: dropping PointerEvent for unknown or closed window_id={}",
                                p.window_id
                            );
                        }
                    }
                    Message::KeyEvent(k) => {
                        // No `window_id`: keyboard input always targets whatever window is
                        // currently focused, which the sink establishes via `PointerEvent` /
                        // `WindowControl::ACTIVATE` (OXPROTO.md §13).
                        let extended = k.flags & key_flag::EXTENDED != 0;
                        let pressed = k.is_pressed();
                        if !input_seen.key {
                            input_seen.key = true;
                            eprintln!(
                                "oxagent: input: first KeyEvent this session: scancode={:#04x} extended={extended} pressed={pressed}",
                                k.scancode
                            );
                        }
                        sink.key_event(k.scancode, extended, pressed);
                    }
                    Message::TextInput(t) => {
                        if text_input_enabled {
                            if !input_seen.text {
                                input_seen.text = true;
                                // The text itself is not logged — it can be anything the user
                                // typed, including a password.
                                eprintln!(
                                    "oxagent: input: first TextInput this session: {} char(s)",
                                    t.text.chars().count()
                                );
                            }
                            sink.text_input(&t.text);
                        } else {
                            eprintln!("oxagent: input: dropping TextInput: TEXT_INPUT not negotiated");
                        }
                    }
                    Message::ModifierSync(m) => {
                        if !input_seen.modifier_sync {
                            input_seen.modifier_sync = true;
                            eprintln!(
                                "oxagent: input: first ModifierSync this session: modifiers={:#06x} locks={:#04x}",
                                m.modifiers, m.locks
                            );
                        }
                        sink.modifier_sync(m.modifiers, m.locks);
                    }
                    // No longer gated by a match guard: `window_control_enabled` is now checked
                    // inside, alongside window resolution, so a rejection either way is logged
                    // instead of silently falling through to the catch-all below.
                    //
                    // Same accepted stale-handle window as `PointerEvent` above — see there.
                    Message::WindowControl(w) => {
                        if !window_control_enabled {
                            eprintln!(
                                "oxagent: input: dropping WindowControl: WINDOW_CONTROL not negotiated"
                            );
                        } else if let Some(stream) = streams.get(&w.window_id) {
                            if !input_seen.window_control {
                                input_seen.window_control = true;
                                eprintln!(
                                    "oxagent: input: first WindowControl this session: window_id={} handle={:#x} action={}",
                                    w.window_id, stream.handle, w.action
                                );
                            }
                            sink.window_control(
                                stream.handle,
                                w.action,
                                w.x,
                                w.y,
                                w.width,
                                w.height,
                            );
                        } else {
                            eprintln!(
                                "oxagent: input: dropping WindowControl for unknown or closed window_id={}",
                                w.window_id
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
                // Taken here, not inside `pump_frames`, so a later window's
                // `tick_to_capture_us` also reflects `sync_windows`' own cost this tick, not
                // just the windows processed before it — the gap `pump_frames`'s doc means by
                // "the tick firing", not just "this function being entered".
                let tick_started = Instant::now();
                sync_windows(writer, source, encoder, registry, streams, params).await?;
                pump_frames(
                    writer,
                    source,
                    encoder,
                    streams,
                    started,
                    tick_started,
                    acks_enabled,
                    codec,
                )
                .await?;
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

/// A [`SourceWindow`]'s show state as a `window_show` value. `minimized` wins over `maximized`
/// in the (physically impossible on Win32 — `IsIconic`/`IsZoomed` are mutually exclusive) case
/// both are somehow set, rather than picking arbitrarily between two contradictory signals.
fn show_state(w: &SourceWindow) -> u8 {
    if w.minimized {
        window_show::MINIMIZED
    } else if w.maximized {
        window_show::MAXIMIZED
    } else {
        window_show::NORMAL
    }
}

/// Diff the platform's window list against what the client has been told.
#[allow(clippy::too_many_arguments)]
async fn sync_windows<W, WR, E>(
    writer: &mut WR,
    source: &mut W,
    encoder: &mut E,
    registry: &mut WindowRegistry,
    streams: &mut HashMap<u32, WindowStream>,
    params: &SessionParams,
) -> io::Result<()>
where
    W: WindowSource,
    WR: AsyncWrite + Unpin,
    E: FrameEncoder,
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
                    show_state: show_state(w),
                    flags: window_flags(w),
                    // Every window's first frame in a session must be a keyframe (OXPROTO.md
                    // §9.1), including — this is that case — a window reported for the first
                    // time to a client that only just attached, whether or not the window
                    // itself is new. `RAW_BGRA` ignores this field entirely.
                    needs_keyframe: true,
                    capture_diag_logged: 0,
                },
            );
            continue;
        }

        // Only report what actually changed — a client that re-lays-out its window on every
        // tick would flicker.
        let Some(stream) = streams.get_mut(&tracked.window_id) else {
            continue;
        };

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

        // `WindowState.flags` carries the *same* bitmask `WindowOpened.flags` does, always the
        // complete current value (OXPROTO.md §11) — computed up front so both the "does
        // WindowState need sending" check here and the "does this specific change force a
        // fresh WindowGeometry" check below can see whether HAS_FRAME in particular flipped,
        // without duplicating `window_flags(w)`.
        let new_flags = window_flags(w);
        let has_frame_changed = (new_flags ^ stream.flags) & window_flag::HAS_FRAME != 0;

        // Sent whenever *either* half changes, always repeating both at their current value —
        // a receiver replaces what it has stored rather than applying a delta (OXPROTO.md
        // §11). A `flags`-only change (RESIZABLE/HAS_FRAME/TOPMOST, with `state` unchanged) is
        // a normal message here, not an edge case: entering full screen is the common case for
        // HAS_FRAME, toggling always-on-top the common case for TOPMOST.
        //
        // Sent *before* `WindowGeometry` below, deliberately: OXPROTO.md §11 says a `HAS_FRAME`
        // change "must" be followed by a fresh `WindowGeometry`, which only makes sense if
        // `WindowState` goes out first — the client would otherwise see a `WindowGeometry` in a
        // coordinate space it has not yet been told changed.
        let new_show_state = show_state(w);
        if stream.show_state != new_show_state || stream.flags != new_flags {
            stream.show_state = new_show_state;
            stream.flags = new_flags;
            send(
                writer,
                &Message::WindowState(WindowState {
                    window_id: tracked.window_id,
                    state: new_show_state,
                    flags: new_flags,
                }),
                channel::WINDOW,
            )
            .await?;
        }

        // Geometry is skipped entirely while minimized: a minimized window's frame bounds are
        // not real geometry (typically 0×0, sometimes an off-screen icon-sized rect), and
        // sending that would make the client resize its native window to nothing. `stream.
        // geometry` is deliberately left untouched too — Windows normally restores a window to
        // the same position/size it had before minimizing, so on restore this comparison finds
        // nothing changed and correctly sends no update; if it truly moved while minimized
        // (rare — some app programmatically repositioned it), the next tick's real comparison
        // catches that once `w.minimized` goes false again.
        if !w.minimized {
            let geometry = (w.x, w.y, w.width, w.height);
            // `has_frame_changed` forces a resend even when the numbers are unchanged: that bit
            // decides whether geometry is client-area or whole-window space
            // (`docs/design/window-decorations.md`), so a flip moves the coordinate space
            // itself and the client must not assume geometry it already has is still correct in
            // the new space until a fresh `WindowGeometry` says so (OXPROTO.md §11).
            if stream.geometry != geometry || has_frame_changed {
                let resolution_changed =
                    (stream.geometry.2, stream.geometry.3) != (w.width, w.height);
                stream.geometry = geometry;
                // A resize invalidates frames already in flight for the old size.
                stream.budget.restart();
                if resolution_changed {
                    // OXPROTO.md §9.1: a coded-size change is a fresh stream start for H.264 —
                    // new SPS/PPS and a keyframe. Harmless to set unconditionally even under
                    // RAW_BGRA, which never reads it.
                    stream.needs_keyframe = true;
                }
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
        }
    }

    let handles: Vec<isize> = live.iter().map(|w| w.handle).collect();
    for gone in registry.retain_live(&handles) {
        if let Some(stream) = streams.remove(&gone.window_id) {
            encoder.forget(stream.handle);
        }
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
///
/// `codec` is the session's negotiated preference — chosen once, at negotiation, and `RAW_BGRA`
/// never involves `encoder` at all, exactly as before this file had one. A session that
/// negotiated `H264`, though, is a per-*session* preference, not a per-window guarantee:
/// `encoder.submit`/`poll` do the real work for each window individually, and if a window's
/// encoder ever reports [`FrameEncoder::failed`] — a real guest run showed construction can fail
/// for reasons outside this crate's control — that one window falls back to sending its captured
/// pixels as `RAW_BGRA` instead, without taking the rest of the session's windows down with it.
/// Every window is judged independently, every tick. `FrameData.codec` always says which path a
/// given message actually took, which is why the per-message codec is computed locally here
/// rather than trusted to equal the outer `codec` parameter.
///
/// Even on the `H264` path, a submitted frame may have nothing ready to send this tick — a real
/// encoder's output can lag its input by a frame or more, so "captured a frame" and "have
/// something to send" are not the same event once a codec is doing real work.
///
/// `tick_started` is when this tick fired, taken by the caller before `sync_windows` — a guest
/// measurement found `capture->encode` costing roughly 6ms more than the encoder's own conversion
/// and compute could account for, and nobody had checked whether any of that was scheduling delay
/// inside this loop rather than the capture call itself. `tick_to_capture_us`, logged per window
/// right before `source.next_frame`, answers that: near-zero for the first window processed each
/// tick, and rising for later ones exactly to the extent earlier windows' work delayed them —
/// which is real, attributable cost, not measurement noise.
#[allow(clippy::too_many_arguments)]
async fn pump_frames<W, WR, E>(
    writer: &mut WR,
    source: &mut W,
    encoder: &mut E,
    streams: &mut HashMap<u32, WindowStream>,
    started: Instant,
    tick_started: Instant,
    acks_enabled: bool,
    codec: u8,
) -> io::Result<()>
where
    W: WindowSource,
    WR: AsyncWrite + Unpin,
    E: FrameEncoder,
{
    // Deterministic order so a window is never starved by map iteration order.
    let mut ids: Vec<u32> = streams.keys().copied().collect();
    ids.sort_unstable();

    for id in ids {
        let Some(stream) = streams.get_mut(&id) else {
            continue;
        };
        // A minimized window has nothing worth capturing — its content is not on screen, and
        // what `Windows.Graphics.Capture` actually hands back for a minimized window (fresh
        // frames, stale ones, or none at all) is not something this file can determine by
        // reading the code, so the safest and simplest answer is to never find out: just don't
        // poll it. `next_frame` is not called at all while `show_state` reads `MINIMIZED`, which
        // also means `WinWindowSource` never even creates the `WindowCapture` for a window that
        // starts out minimized, and — since the existing capture is left alone, not torn down,
        // for one that *becomes* minimized — resumes instantly with no re-creation stutter on
        // restore.
        if stream.show_state == window_show::MINIMIZED {
            continue;
        }
        // Without acks there is no feedback to pace against, so send every capture and let the
        // transport apply back-pressure.
        if acks_enabled && !stream.budget.has_headroom() {
            // Still capture: `on_captured` displaces the stale frame rather than queueing.
        }
        let tick_to_capture_us = tick_started.elapsed().as_micros() as u64;
        let Some(frame) = source.next_frame(stream.handle) else {
            continue;
        };
        let captured_us = elapsed_us(started);

        if stream.capture_diag_logged < CAPTURE_DIAGNOSTIC_FRAME_LIMIT {
            stream.capture_diag_logged += 1;
            eprintln!(
                "oxagent: capture: window={:#x} frame={} tick_to_capture_us={tick_to_capture_us}",
                stream.handle, stream.capture_diag_logged
            );
        }

        // Submitted before the `failed` check below so a construction failure discovered on
        // *this* call already routes this very frame through the fallback, rather than losing
        // one frame while `failed` catches up on the next tick.
        if codec == oxproto::codec::H264 {
            encoder.submit(stream.handle, &frame, stream.needs_keyframe);
        }

        let (msg_codec, flags, width, height, data, encoded_us) =
            if codec == oxproto::codec::H264 && !encoder.failed(stream.handle) {
                let Some(encoded) = encoder.poll(stream.handle) else {
                    // Nothing ready yet this tick: no `frame_id` is spent, no `FrameData` is
                    // sent. The next tick tries again — `submit` above already handed the
                    // encoder this frame's content, so nothing is lost by waiting.
                    continue;
                };
                if encoded.keyframe {
                    stream.needs_keyframe = false;
                }
                let flags = if encoded.keyframe {
                    frame_flag::KEYFRAME
                } else {
                    0
                };
                // The *coded* size, not the captured size: an encoder that padded an odd
                // capture dimension to the even size NV12 requires reports that padded size
                // here, so `FrameData` never disagrees with what its own bitstream's SPS says.
                (
                    oxproto::codec::H264,
                    flags,
                    encoded.width,
                    encoded.height,
                    encoded.data,
                    elapsed_us(started),
                )
            } else {
                // `RAW_BGRA`, either because the session negotiated it outright or because this
                // one window's encoder just reported `failed` (see this function's doc) —
                // either way this window's own pixels, sent uncoded. Every frame is trivially a
                // "keyframe" (there is nothing to reference), and capture and "encode" complete
                // at the same instant since there is no separate encode step.
                (
                    oxproto::codec::RAW_BGRA,
                    frame_flag::KEYFRAME,
                    frame.width,
                    frame.height,
                    frame.data,
                    captured_us,
                )
            };

        let frame_id = stream.budget.on_captured();
        let body = Message::FrameData(FrameData {
            window_id: id,
            frame_id,
            codec: msg_codec,
            flags,
            width,
            height,
            captured_us,
            encoded_us,
            data,
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
    use crate::encode::EncodedFrame;
    use crate::handshake::RAW_BGRA_ONLY;
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

    /// A single window with real geometry, for the minimize/restore tests below.
    fn minimizable_window() -> SourceWindow {
        SourceWindow {
            handle: 0x2000,
            pid: 7,
            app_id: "app.exe".into(),
            title: "Title".into(),
            x: 5,
            y: 5,
            width: 100,
            height: 50,
            dpi: 96,
            minimized: false,
            maximized: false,
            resizable: true,
            has_frame: true,
            topmost: false,
        }
    }

    /// A `WindowSource` reporting a single window whose fields the test can mutate between
    /// ticks (minimize, restore, resize, ...) — `FakeSource`'s window list is fixed at
    /// construction, which cannot express a transition happening mid-session.
    struct MutableSource(std::sync::Arc<std::sync::Mutex<SourceWindow>>);

    impl WindowSource for MutableSource {
        fn live_windows(&mut self) -> Vec<SourceWindow> {
            vec![self.0.lock().unwrap().clone()]
        }

        fn next_frame(&mut self, _handle: isize) -> Option<SourceFrame> {
            None
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

    /// A [`FrameEncoder`] that never has anything ready — for tests exercising `RAW_BGRA` (which
    /// never calls it) or anything else where encoding is not the point.
    struct NoopEncoder;
    impl FrameEncoder for NoopEncoder {
        fn submit(&mut self, _: isize, _: &SourceFrame, _: bool) {}
        fn poll(&mut self, _: isize) -> Option<EncodedFrame> {
            None
        }
        fn forget(&mut self, _: isize) {}
        fn failed(&self, _: isize) -> bool {
            false
        }
    }

    /// One recorded call to a [`FrameEncoder`], for asserting exactly what `pump_frames` asked
    /// it to do.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum EncoderCall {
        Submit { handle: isize, force_keyframe: bool },
        Forget { handle: isize },
    }

    /// A [`FrameEncoder`] that "encodes" synchronously — whatever is submitted for a window is
    /// immediately available from the very next `poll` for that window — and reports every call
    /// over a channel. `coded_size`, when set, overrides the width/height the encoder reports
    /// for its output, independent of the input frame's own size — the same way a real encoder
    /// padding an odd capture dimension to the even size NV12 requires would. `failing`, when a
    /// handle is inserted into it, makes `failed()` report `true` for exactly that handle — for
    /// tests of the per-window `RAW_BGRA` fallback (`crate::serve::pump_frames`'s doc); `submit`/
    /// `poll` still record and behave normally for a failing handle, since the fake does not
    /// know *why* a real encoder would have failed, only that this test wants it to have.
    struct RecordingEncoder {
        calls: mpsc::UnboundedSender<EncoderCall>,
        pending: HashMap<isize, EncodedFrame>,
        coded_size: Option<(u16, u16)>,
        failing: std::collections::HashSet<isize>,
    }

    impl RecordingEncoder {
        fn new(calls: mpsc::UnboundedSender<EncoderCall>) -> Self {
            Self {
                calls,
                pending: HashMap::new(),
                coded_size: None,
                failing: std::collections::HashSet::new(),
            }
        }
    }

    impl FrameEncoder for RecordingEncoder {
        fn submit(&mut self, handle: isize, frame: &SourceFrame, force_keyframe: bool) {
            let _ = self.calls.send(EncoderCall::Submit {
                handle,
                force_keyframe,
            });
            let (width, height) = self.coded_size.unwrap_or((frame.width, frame.height));
            self.pending.insert(
                handle,
                EncodedFrame {
                    data: vec![0xAB], // placeholder bitstream bytes; content is not the point
                    keyframe: force_keyframe,
                    width,
                    height,
                },
            );
        }

        fn poll(&mut self, handle: isize) -> Option<EncodedFrame> {
            self.pending.remove(&handle)
        }

        fn forget(&mut self, handle: isize) {
            let _ = self.calls.send(EncoderCall::Forget { handle });
        }

        fn failed(&self, handle: isize) -> bool {
            self.failing.contains(&handle)
        }
    }

    /// Like [`MutableSource`], but also produces a frame matching the window's current size on
    /// every poll while it is not minimized — for tests that need to drive the encoder, not
    /// just window lifecycle messages.
    struct MutableFrameSource(std::sync::Arc<std::sync::Mutex<SourceWindow>>);

    impl WindowSource for MutableFrameSource {
        fn live_windows(&mut self) -> Vec<SourceWindow> {
            vec![self.0.lock().unwrap().clone()]
        }

        fn next_frame(&mut self, _handle: isize) -> Option<SourceFrame> {
            let w = self.0.lock().unwrap();
            if w.minimized {
                return None;
            }
            Some(SourceFrame {
                width: w.width,
                height: w.height,
                data: vec![0xAB; 4],
            })
        }
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

    /// A `pre_auth_deadline` far enough out that a test exercising anything other than the
    /// timeout itself never comes close to hitting it.
    fn far_deadline() -> Instant {
        Instant::now() + Duration::from_secs(30)
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

    #[test]
    fn show_state_prioritizes_minimized_over_maximized() {
        use oxproto::message::window::window_show;

        assert_eq!(show_state(&plain_window()), window_show::NORMAL);
        assert_eq!(
            show_state(&SourceWindow {
                minimized: true,
                ..plain_window()
            }),
            window_show::MINIMIZED
        );
        assert_eq!(
            show_state(&SourceWindow {
                maximized: true,
                ..plain_window()
            }),
            window_show::MAXIMIZED
        );
        assert_eq!(
            show_state(&SourceWindow {
                minimized: true,
                maximized: true,
                ..plain_window()
            }),
            window_show::MINIMIZED,
            "minimized wins in the physically-impossible case both are set"
        );
    }

    fn hello(token: &str) -> Message {
        hello_with_features(token, feature::FRAME_ACK)
    }

    /// Like [`hello`], but with a caller-chosen feature set — for tests where negotiating (or
    /// not negotiating) `TEXT_INPUT`/`WINDOW_CONTROL` is the point.
    fn hello_with_features(token: &str, features: u64) -> Message {
        hello_with(token, features, vec![oxproto::codec::RAW_BGRA])
    }

    /// Like [`hello`], with a caller-chosen feature set *and* codec offer — for tests where
    /// which codec the client asks for is the point.
    fn hello_with(token: &str, features: u64, codecs: Vec<u8>) -> Message {
        Message::ClientHello(ClientHello {
            version_min: 1,
            version_max: 1,
            features,
            auth_token: token.into(),
            client_name: "test".into(),
            codecs,
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
            let mut encoder = NoopEncoder;
            let session_slot = Semaphore::new(1);
            run_session(
                agent,
                &mut source,
                &mut sink,
                &mut encoder,
                SessionParams {
                    target_fps: 240,
                    max_frames_in_flight: 2,
                },
                77,
                "secret",
                RAW_BGRA_ONLY,
                far_deadline(),
                &session_slot,
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
            let mut encoder = NoopEncoder;
            let session_slot = Semaphore::new(1);
            run_session(
                agent,
                &mut source,
                &mut sink,
                &mut encoder,
                SessionParams::default(),
                1,
                "secret",
                RAW_BGRA_ONLY,
                far_deadline(),
                &session_slot,
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
            let mut encoder = NoopEncoder;
            let session_slot = Semaphore::new(1);
            run_session(
                agent,
                &mut source,
                &mut sink,
                &mut encoder,
                SessionParams {
                    target_fps: 240,
                    max_frames_in_flight: 2,
                },
                5,
                "secret",
                RAW_BGRA_ONLY,
                far_deadline(),
                &session_slot,
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
            let mut encoder = NoopEncoder;
            let session_slot = Semaphore::new(1);
            run_session(
                agent,
                &mut source,
                &mut sink,
                &mut encoder,
                SessionParams::default(),
                3,
                "secret",
                RAW_BGRA_ONLY,
                far_deadline(),
                &session_slot,
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
            let mut encoder = NoopEncoder;
            let session_slot = Semaphore::new(1);
            run_session(
                agent,
                &mut source,
                &mut sink,
                &mut encoder,
                SessionParams::default(),
                4,
                "secret",
                RAW_BGRA_ONLY,
                far_deadline(),
                &session_slot,
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
                let mut encoder = NoopEncoder;
                let session_slot = Semaphore::new(1);
                run_session(
                    agent,
                    &mut source,
                    &mut sink,
                    &mut encoder,
                    SessionParams::default(),
                    1,
                    "secret",
                    RAW_BGRA_ONLY,
                    far_deadline(),
                    &session_slot,
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
            let mut encoder = NoopEncoder;
            let session_slot = Semaphore::new(1);
            run_session(
                agent,
                &mut source,
                &mut sink,
                &mut encoder,
                SessionParams::default(),
                9,
                "secret",
                RAW_BGRA_ONLY,
                far_deadline(),
                &session_slot,
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

    #[tokio::test]
    async fn minimizing_reports_window_state_not_a_close_and_reopen() {
        use oxproto::message::window::window_show;

        let state = std::sync::Arc::new(std::sync::Mutex::new(minimizable_window()));
        let (mut client, agent) = tokio::io::duplex(64 * 1024);

        let agent_task = tokio::spawn({
            let state = std::sync::Arc::clone(&state);
            async move {
                let mut source = MutableSource(state);
                let mut sink = NoopSink;
                let mut encoder = NoopEncoder;
                let session_slot = Semaphore::new(1);
                run_session(
                    agent,
                    &mut source,
                    &mut sink,
                    &mut encoder,
                    SessionParams {
                        target_fps: 240,
                        max_frames_in_flight: 2,
                    },
                    11,
                    "secret",
                    RAW_BGRA_ONLY,
                    far_deadline(),
                    &session_slot,
                )
                .await
            }
        });

        let mut r = Reassembler::new();
        write_message(&mut client, &hello("secret"), channel::CONTROL)
            .await
            .unwrap();
        let opened = wait_for_window_opened(&mut client, &mut r).await;

        state.lock().unwrap().minimized = true;

        // Before the fix, `live_windows()` dropped a minimized window entirely, so the driver
        // saw it vanish from the live set and reported `WindowClosed` — then `WindowOpened`
        // again on restore, losing everything the client was tracking about it. It must instead
        // stay the *same* window and just change show state.
        let mut saw_state = None;
        for _ in 0..32 {
            match read_message(&mut client, &mut r).await.unwrap().unwrap() {
                Message::WindowState(s) => {
                    saw_state = Some(s);
                    break;
                }
                Message::WindowClosed(_) => panic!("minimizing must not close the window"),
                Message::WindowOpened(_) => panic!("minimizing must not re-open the window"),
                _ => continue,
            }
        }
        let state_msg = saw_state.expect("a WindowState should have been sent");
        assert_eq!(state_msg.window_id, opened.window_id);
        assert_eq!(state_msg.state, window_show::MINIMIZED);

        drop(client);
        let _ = agent_task.await;
    }

    #[tokio::test]
    async fn minimizing_and_restoring_sends_no_degenerate_geometry() {
        use oxproto::message::window::window_show;

        let state = std::sync::Arc::new(std::sync::Mutex::new(minimizable_window()));
        let (mut client, agent) = tokio::io::duplex(64 * 1024);

        let agent_task = tokio::spawn({
            let state = std::sync::Arc::clone(&state);
            async move {
                let mut source = MutableSource(state);
                let mut sink = NoopSink;
                let mut encoder = NoopEncoder;
                let session_slot = Semaphore::new(1);
                run_session(
                    agent,
                    &mut source,
                    &mut sink,
                    &mut encoder,
                    SessionParams {
                        target_fps: 240,
                        max_frames_in_flight: 2,
                    },
                    12,
                    "secret",
                    RAW_BGRA_ONLY,
                    far_deadline(),
                    &session_slot,
                )
                .await
            }
        });

        let mut r = Reassembler::new();
        write_message(&mut client, &hello("secret"), channel::CONTROL)
            .await
            .unwrap();
        let opened = wait_for_window_opened(&mut client, &mut r).await;
        let (orig_width, orig_height) = (opened.width, opened.height);

        // Minimize: the real platform would now report ~0×0 (`describe_window` allows that
        // while `IsIconic`); simulated directly here since this test is platform-independent.
        {
            let mut w = state.lock().unwrap();
            w.minimized = true;
            w.width = 0;
            w.height = 0;
        }

        let mut saw_minimized = false;
        for _ in 0..32 {
            match read_message(&mut client, &mut r).await.unwrap().unwrap() {
                Message::WindowState(s) if s.state == window_show::MINIMIZED => {
                    saw_minimized = true;
                    break;
                }
                Message::WindowGeometry(g) => {
                    panic!("must never send geometry while minimized, got {g:?}")
                }
                _ => continue,
            }
        }
        assert!(saw_minimized, "the minimize transition should be reported");

        // Restore to exactly the original geometry — Windows' normal behavior, and the case
        // that must produce no `WindowGeometry` at all, since nothing about the real geometry
        // changed across the round trip.
        {
            let mut w = state.lock().unwrap();
            w.minimized = false;
            w.width = orig_width;
            w.height = orig_height;
        }

        let mut saw_restored = false;
        for _ in 0..32 {
            match read_message(&mut client, &mut r).await.unwrap().unwrap() {
                Message::WindowState(s) if s.state == window_show::NORMAL => {
                    saw_restored = true;
                    break;
                }
                Message::WindowGeometry(g) => {
                    panic!("restoring to the same geometry must not re-announce it, got {g:?}")
                }
                _ => continue,
            }
        }
        assert!(saw_restored, "the restore transition should be reported");

        drop(client);
        let _ = agent_task.await;
    }

    #[tokio::test]
    async fn frames_are_not_pumped_for_a_minimized_window() {
        /// Panics if asked for a frame while the shared window state says minimized — proving
        /// `pump_frames` itself skips a minimized window, rather than this test merely
        /// happening not to observe a `FrameData` for one.
        struct PanicsIfPolledWhileMinimized(std::sync::Arc<std::sync::Mutex<SourceWindow>>);
        impl WindowSource for PanicsIfPolledWhileMinimized {
            fn live_windows(&mut self) -> Vec<SourceWindow> {
                vec![self.0.lock().unwrap().clone()]
            }
            fn next_frame(&mut self, _handle: isize) -> Option<SourceFrame> {
                assert!(
                    !self.0.lock().unwrap().minimized,
                    "next_frame must never be called for a minimized window"
                );
                Some(SourceFrame {
                    width: 1,
                    height: 1,
                    data: vec![0xAB],
                })
            }
        }

        let state = std::sync::Arc::new(std::sync::Mutex::new(minimizable_window()));
        let (mut client, agent) = tokio::io::duplex(64 * 1024);

        let agent_task = tokio::spawn({
            let state = std::sync::Arc::clone(&state);
            async move {
                let mut source = PanicsIfPolledWhileMinimized(state);
                let mut sink = NoopSink;
                let mut encoder = NoopEncoder;
                let session_slot = Semaphore::new(1);
                run_session(
                    agent,
                    &mut source,
                    &mut sink,
                    &mut encoder,
                    SessionParams {
                        target_fps: 240,
                        max_frames_in_flight: 2,
                    },
                    13,
                    "secret",
                    RAW_BGRA_ONLY,
                    far_deadline(),
                    &session_slot,
                )
                .await
            }
        });

        let mut r = Reassembler::new();
        write_message(&mut client, &hello("secret"), channel::CONTROL)
            .await
            .unwrap();
        let _ = wait_for_window_opened(&mut client, &mut r).await;

        // Sanity: frames really do flow while not minimized, so the mock proves something.
        let mut saw_frame = false;
        for _ in 0..32 {
            match read_message(&mut client, &mut r).await.unwrap().unwrap() {
                Message::FrameData(_) => {
                    saw_frame = true;
                    break;
                }
                _ => continue,
            }
        }
        assert!(saw_frame, "frames should flow for a non-minimized window");

        state.lock().unwrap().minimized = true;

        // Give the driver several ticks (240 fps ⇒ ~4ms each) to act on the new state without
        // reading anything: once minimized, no further messages are expected at all (geometry
        // is suppressed and nothing else changes), so waiting on `read_message` here would just
        // hang. If `pump_frames` called `next_frame` while minimized, the mock would already
        // have panicked inside the spawned task by the time this returns.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        drop(client);
        let result = agent_task.await;
        assert!(
            result.is_ok(),
            "the session task must not have panicked: {result:?}"
        );
    }

    #[tokio::test]
    async fn a_silent_peer_is_dropped_after_the_pre_auth_deadline() {
        // The attack this closes: connect and send nothing at all. Before the deadline existed,
        // `negotiate`'s `ClientHello` read simply waited forever.
        let (client, agent) = tokio::io::duplex(4096);
        let deadline = Instant::now() + Duration::from_millis(50);
        let session_slot = Semaphore::new(1);
        let mut source = FakeSource::one_window(0);
        let mut sink = NoopSink;
        let mut encoder = NoopEncoder;

        let started = Instant::now();
        let result = run_session(
            agent,
            &mut source,
            &mut sink,
            &mut encoder,
            SessionParams::default(),
            1,
            "secret",
            RAW_BGRA_ONLY,
            deadline,
            &session_slot,
        )
        .await;
        let elapsed = started.elapsed();

        assert!(
            matches!(result, Err(HandshakeError::Timeout)),
            "got {result:?}"
        );
        // Generous slack over the 50ms deadline for scheduling jitter, but nowhere near what a
        // bug that ignored the deadline and blocked forever would look like.
        assert!(
            elapsed < Duration::from_secs(2),
            "took {elapsed:?}, should have returned promptly after the deadline"
        );

        drop(client); // the silent peer: still connected, never sent a byte.
    }

    #[tokio::test]
    async fn a_hanging_connection_does_not_block_a_second_one() {
        // Simulates two connections handled the way `crate::win::run_agent` now handles them:
        // each in its own task. Connection A never sends anything and has a short deadline of
        // its own; connection B authenticates normally. Before per-connection concurrency, a
        // single stalled accept loop would have made B wait for A's slot even to be attempted —
        // now B must complete while A is still (deliberately) hanging.
        let (client_a, agent_a) = tokio::io::duplex(4096);
        let (mut client_b, agent_b) = tokio::io::duplex(4096);

        let session_slot = std::sync::Arc::new(Semaphore::new(1));
        let deadline_a = Instant::now() + Duration::from_millis(300);
        let deadline_b = far_deadline();

        let _task_a = tokio::spawn({
            let session_slot = std::sync::Arc::clone(&session_slot);
            async move {
                let mut source = FakeSource::one_window(0);
                let mut sink = NoopSink;
                let mut encoder = NoopEncoder;
                run_session(
                    agent_a,
                    &mut source,
                    &mut sink,
                    &mut encoder,
                    SessionParams::default(),
                    1,
                    "secret",
                    RAW_BGRA_ONLY,
                    deadline_a,
                    &session_slot,
                )
                .await
            }
        });

        let task_b = tokio::spawn({
            let session_slot = std::sync::Arc::clone(&session_slot);
            async move {
                let mut source = FakeSource::one_window(0);
                let mut sink = NoopSink;
                let mut encoder = NoopEncoder;
                run_session(
                    agent_b,
                    &mut source,
                    &mut sink,
                    &mut encoder,
                    SessionParams::default(),
                    2,
                    "secret",
                    RAW_BGRA_ONLY,
                    deadline_b,
                    &session_slot,
                )
                .await
            }
        });

        write_message(&mut client_b, &hello("secret"), channel::CONTROL)
            .await
            .unwrap();
        let mut rb = Reassembler::new();
        // Bounded well under A's 300ms deadline: if connections were still handled one at a
        // time, B would not even start being served until A's task finished.
        let sh = tokio::time::timeout(Duration::from_millis(150), async {
            loop {
                if let Some(msg) = read_message(&mut client_b, &mut rb).await.unwrap() {
                    return msg;
                }
            }
        })
        .await
        .expect("B must not wait on A's hanging connection");
        assert!(matches!(sh, Message::ServerHello(_)));

        task_b.abort();
        drop(client_a);
        drop(client_b);
    }

    #[tokio::test]
    async fn a_second_authenticated_client_is_rejected_while_one_is_active() {
        // Requirement: connections are now handled concurrently, but exactly one *authenticated*
        // session may drive windows at a time. A second one must be turned away with a real
        // `Error` + `Close`, not queued and not allowed to run alongside the first.
        let session_slot = std::sync::Arc::new(Semaphore::new(1));
        let deadline = far_deadline();

        let (mut client_a, agent_a) = tokio::io::duplex(64 * 1024);
        let task_a = tokio::spawn({
            let session_slot = std::sync::Arc::clone(&session_slot);
            async move {
                let mut source = FakeSource::one_window(0);
                let mut sink = NoopSink;
                let mut encoder = NoopEncoder;
                run_session(
                    agent_a,
                    &mut source,
                    &mut sink,
                    &mut encoder,
                    SessionParams {
                        target_fps: 240,
                        max_frames_in_flight: 2,
                    },
                    1,
                    "secret",
                    RAW_BGRA_ONLY,
                    deadline,
                    &session_slot,
                )
                .await
            }
        });

        let mut ra = Reassembler::new();
        write_message(&mut client_a, &hello("secret"), channel::CONTROL)
            .await
            .unwrap();
        let sh_a = read_message(&mut client_a, &mut ra).await.unwrap().unwrap();
        assert!(matches!(sh_a, Message::ServerHello(_)));
        // Give A's task a chance to run past the (synchronous, no further `.await` before it)
        // `try_acquire` that follows sending `ServerHello`, so the permit is actually held by
        // the time B tries for it below — otherwise this test would be racing the scheduler.
        tokio::time::sleep(Duration::from_millis(10)).await;

        let (mut client_b, agent_b) = tokio::io::duplex(64 * 1024);
        write_message(&mut client_b, &hello("secret"), channel::CONTROL)
            .await
            .unwrap();
        let mut source_b = FakeSource::one_window(0);
        let mut sink_b = NoopSink;
        let mut encoder_b = NoopEncoder;
        let result_b = run_session(
            agent_b,
            &mut source_b,
            &mut sink_b,
            &mut encoder_b,
            SessionParams::default(),
            2,
            "secret",
            RAW_BGRA_ONLY,
            deadline,
            &session_slot,
        )
        .await;
        assert!(
            matches!(result_b, Err(HandshakeError::Busy)),
            "got {result_b:?}"
        );

        let mut rb = Reassembler::new();
        let sh_b = read_message(&mut client_b, &mut rb).await.unwrap().unwrap();
        assert!(
            matches!(sh_b, Message::ServerHello(_)),
            "B does authenticate — it just cannot run a session"
        );
        let err_b = read_message(&mut client_b, &mut rb).await.unwrap().unwrap();
        assert!(
            matches!(
                err_b,
                Message::Error(ProtoError {
                    code: error_code::SESSION_BUSY,
                    ..
                })
            ),
            "got {err_b:?}"
        );
        let close_b = read_message(&mut client_b, &mut rb).await.unwrap().unwrap();
        assert!(matches!(close_b, Message::Close(_)), "got {close_b:?}");

        drop(client_a);
        drop(client_b);
        let _ = task_a.await;
    }

    #[tokio::test]
    async fn h264_forces_a_keyframe_for_the_first_frame_and_after_a_resize() {
        let state = std::sync::Arc::new(std::sync::Mutex::new(minimizable_window()));
        let (mut client, agent) = tokio::io::duplex(64 * 1024);
        let (tx, mut rx) = mpsc::unbounded_channel();

        let agent_task = tokio::spawn({
            let state = std::sync::Arc::clone(&state);
            async move {
                let mut source = MutableFrameSource(state);
                let mut sink = NoopSink;
                let mut encoder = RecordingEncoder::new(tx);
                let session_slot = Semaphore::new(1);
                run_session(
                    agent,
                    &mut source,
                    &mut sink,
                    &mut encoder,
                    SessionParams {
                        target_fps: 240,
                        max_frames_in_flight: 2,
                    },
                    30,
                    "secret",
                    &[oxproto::codec::RAW_BGRA, oxproto::codec::H264],
                    far_deadline(),
                    &session_slot,
                )
                .await
            }
        });

        let mut r = Reassembler::new();
        write_message(
            &mut client,
            &hello_with("secret", feature::FRAME_ACK, vec![oxproto::codec::H264]),
            channel::CONTROL,
        )
        .await
        .unwrap();
        let sh = read_message(&mut client, &mut r).await.unwrap().unwrap();
        let Message::ServerHello(sh) = sh else {
            panic!("expected ServerHello, got {sh:?}")
        };
        assert_eq!(sh.codec, oxproto::codec::H264);
        let _opened = wait_for_window_opened(&mut client, &mut r).await;

        let first = rx.recv().await.expect("the encoder should be submitted to");
        assert_eq!(
            first,
            EncoderCall::Submit {
                handle: 0x2000,
                force_keyframe: true,
            },
            "a window's first frame in a session must be forced as a keyframe"
        );

        // The resulting FrameData must actually carry the KEYFRAME flag and the H264 codec id.
        let mut saw_keyframe = false;
        for _ in 0..32 {
            match read_message(&mut client, &mut r).await.unwrap().unwrap() {
                Message::FrameData(f) => {
                    assert_eq!(f.codec, oxproto::codec::H264);
                    assert_ne!(f.flags & frame_flag::KEYFRAME, 0);
                    saw_keyframe = true;
                    break;
                }
                _ => continue,
            }
        }
        assert!(saw_keyframe, "the first FrameData must be a keyframe");

        // A coded-size change (OXPROTO.md §9.1) must force a fresh keyframe too.
        state.lock().unwrap().width = 200;

        let mut saw_forced_keyframe_again = false;
        for _ in 0..64 {
            match rx.recv().await.unwrap() {
                EncoderCall::Submit {
                    force_keyframe: true,
                    ..
                } => {
                    saw_forced_keyframe_again = true;
                    break;
                }
                _ => continue,
            }
        }
        assert!(
            saw_forced_keyframe_again,
            "a coded-size change must force a fresh keyframe"
        );

        drop(client);
        let _ = agent_task.await;
    }

    #[tokio::test]
    async fn raw_bgra_sessions_never_touch_the_encoder() {
        let (mut client, agent) = tokio::io::duplex(64 * 1024);
        let (tx, mut rx) = mpsc::unbounded_channel();

        let agent_task = tokio::spawn(async move {
            let mut source = FakeSource::one_window(4);
            let mut sink = NoopSink;
            let mut encoder = RecordingEncoder::new(tx);
            let session_slot = Semaphore::new(1);
            run_session(
                agent,
                &mut source,
                &mut sink,
                &mut encoder,
                SessionParams {
                    target_fps: 240,
                    max_frames_in_flight: 2,
                },
                31,
                "secret",
                RAW_BGRA_ONLY,
                far_deadline(),
                &session_slot,
            )
            .await
        });

        let mut r = Reassembler::new();
        write_message(&mut client, &hello("secret"), channel::CONTROL)
            .await
            .unwrap();
        let mut frames = 0;
        while frames < 2 {
            match read_message(&mut client, &mut r).await.unwrap() {
                Some(Message::FrameData(f)) => {
                    assert_eq!(f.codec, oxproto::codec::RAW_BGRA);
                    frames += 1;
                }
                Some(_) => continue,
                None => continue,
            }
        }

        assert!(
            rx.try_recv().is_err(),
            "a RAW_BGRA session must never call the encoder"
        );

        drop(client);
        let _ = agent_task.await;
    }

    #[tokio::test]
    async fn a_window_whose_encoder_failed_falls_back_to_raw_bgra_within_an_h264_session() {
        // `FakeSource::one_window` always uses handle 0x1000 — see there.
        let (mut client, agent) = tokio::io::duplex(64 * 1024);
        let (tx, _rx) = mpsc::unbounded_channel();

        let agent_task = tokio::spawn(async move {
            let mut source = FakeSource::one_window(8);
            let mut sink = NoopSink;
            let mut encoder = RecordingEncoder::new(tx);
            encoder.failing.insert(0x1000);
            let session_slot = Semaphore::new(1);
            run_session(
                agent,
                &mut source,
                &mut sink,
                &mut encoder,
                SessionParams {
                    target_fps: 240,
                    max_frames_in_flight: 2,
                },
                32,
                "secret",
                &[oxproto::codec::RAW_BGRA, oxproto::codec::H264],
                far_deadline(),
                &session_slot,
            )
            .await
        });

        let mut r = Reassembler::new();
        write_message(
            &mut client,
            &hello_with("secret", feature::FRAME_ACK, vec![oxproto::codec::H264]),
            channel::CONTROL,
        )
        .await
        .unwrap();
        let sh = read_message(&mut client, &mut r).await.unwrap().unwrap();
        let Message::ServerHello(sh) = sh else {
            panic!("expected ServerHello, got {sh:?}")
        };
        assert_eq!(
            sh.codec,
            oxproto::codec::H264,
            "the session negotiates H264 as usual — the fallback is per window, not per session"
        );

        let mut saw_raw = false;
        for _ in 0..32 {
            match read_message(&mut client, &mut r).await.unwrap().unwrap() {
                Message::FrameData(f) => {
                    assert_eq!(
                        f.codec,
                        oxproto::codec::RAW_BGRA,
                        "a window whose encoder failed must send RAW_BGRA even though the \
                         session negotiated H264"
                    );
                    assert_ne!(
                        f.flags & frame_flag::KEYFRAME,
                        0,
                        "every RAW_BGRA frame is trivially a keyframe"
                    );
                    saw_raw = true;
                    break;
                }
                _ => continue,
            }
        }
        assert!(saw_raw, "expected at least one FrameData");

        drop(client);
        let _ = agent_task.await;
    }

    #[tokio::test]
    async fn h264_frame_data_reports_the_encoders_coded_size_not_the_captured_size() {
        let state = std::sync::Arc::new(std::sync::Mutex::new(minimizable_window())); // 100x50
        let (mut client, agent) = tokio::io::duplex(64 * 1024);
        let (tx, _rx) = mpsc::unbounded_channel();

        let agent_task = tokio::spawn({
            let state = std::sync::Arc::clone(&state);
            async move {
                let mut source = MutableFrameSource(state);
                let mut sink = NoopSink;
                let mut encoder = RecordingEncoder::new(tx);
                // Simulates an encoder that padded an odd capture dimension to the even size
                // NV12 requires — deliberately different from the captured 100x50.
                encoder.coded_size = Some((102, 50));
                let session_slot = Semaphore::new(1);
                run_session(
                    agent,
                    &mut source,
                    &mut sink,
                    &mut encoder,
                    SessionParams {
                        target_fps: 240,
                        max_frames_in_flight: 2,
                    },
                    32,
                    "secret",
                    &[oxproto::codec::RAW_BGRA, oxproto::codec::H264],
                    far_deadline(),
                    &session_slot,
                )
                .await
            }
        });

        let mut r = Reassembler::new();
        write_message(
            &mut client,
            &hello_with("secret", feature::FRAME_ACK, vec![oxproto::codec::H264]),
            channel::CONTROL,
        )
        .await
        .unwrap();
        let sh = read_message(&mut client, &mut r).await.unwrap().unwrap();
        assert!(matches!(sh, Message::ServerHello(_)));
        let _opened = wait_for_window_opened(&mut client, &mut r).await;

        let mut saw = false;
        for _ in 0..32 {
            match read_message(&mut client, &mut r).await.unwrap().unwrap() {
                Message::FrameData(f) => {
                    assert_eq!(
                        (f.width, f.height),
                        (102, 50),
                        "must report the encoder's coded size, not the captured 100x50"
                    );
                    saw = true;
                    break;
                }
                _ => continue,
            }
        }
        assert!(saw, "expected at least one FrameData");

        drop(client);
        let _ = agent_task.await;
    }

    #[tokio::test]
    async fn a_has_frame_change_forces_a_fresh_window_geometry_even_without_a_resize() {
        use oxproto::message::window::window_flag;

        // `minimizable_window()` starts `has_frame: true` at (5,5) 100x50.
        let state = std::sync::Arc::new(std::sync::Mutex::new(minimizable_window()));
        let (mut client, agent) = tokio::io::duplex(64 * 1024);

        let agent_task = tokio::spawn({
            let state = std::sync::Arc::clone(&state);
            async move {
                let mut source = MutableSource(state);
                let mut sink = NoopSink;
                let mut encoder = NoopEncoder;
                let session_slot = Semaphore::new(1);
                run_session(
                    agent,
                    &mut source,
                    &mut sink,
                    &mut encoder,
                    SessionParams {
                        target_fps: 240,
                        max_frames_in_flight: 2,
                    },
                    33,
                    "secret",
                    RAW_BGRA_ONLY,
                    far_deadline(),
                    &session_slot,
                )
                .await
            }
        });

        let mut r = Reassembler::new();
        write_message(&mut client, &hello("secret"), channel::CONTROL)
            .await
            .unwrap();
        let opened = wait_for_window_opened(&mut client, &mut r).await;
        assert_ne!(opened.flags & window_flag::HAS_FRAME, 0);

        // Flip HAS_FRAME without touching x/y/width/height at all.
        state.lock().unwrap().has_frame = false;

        let mut saw_state = false;
        for _ in 0..32 {
            match read_message(&mut client, &mut r).await.unwrap().unwrap() {
                Message::WindowState(s) => {
                    assert_eq!(s.window_id, opened.window_id);
                    assert_eq!(
                        s.flags & window_flag::HAS_FRAME,
                        0,
                        "HAS_FRAME must be reported clear now"
                    );
                    saw_state = true;
                    break;
                }
                _ => continue,
            }
        }
        assert!(
            saw_state,
            "the flags change must be reported via WindowState"
        );

        // Must be followed by a fresh WindowGeometry, even though x/y/width/height are
        // unchanged — HAS_FRAME moves the coordinate space itself (OXPROTO.md §11), and a
        // client must not assume geometry it already has is still correct in the new space.
        let mut saw_geometry = false;
        for _ in 0..32 {
            match read_message(&mut client, &mut r).await.unwrap().unwrap() {
                Message::WindowGeometry(g) => {
                    assert_eq!(g.window_id, opened.window_id);
                    assert_eq!(
                        (g.x, g.y, g.width, g.height),
                        (opened.x, opened.y, opened.width, opened.height)
                    );
                    saw_geometry = true;
                    break;
                }
                _ => continue,
            }
        }
        assert!(
            saw_geometry,
            "HAS_FRAME changing must force a fresh WindowGeometry"
        );

        drop(client);
        let _ = agent_task.await;
    }
}
