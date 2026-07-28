//! `oxclient` — connect to an `oxagent` over pinned TLS and present remote windows.
//!
//! `--headless` keeps the original event-printing bring-up path for environments without a
//! display server.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::env;
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use oxclient::decode::{self, WindowDecoders};
use oxclient::geometry::GeometrySync;
use oxclient::session::{ClientSession, SessionConfig};
use oxclient::{ClientEvent, ModelChange, RemoteWindow, WindowModel};
use oxdisplay::{CommandSender, CpuPresenter, DisplayCommand, DisplayEvent, WindowSpec};
use oxproto::message::input::{key_flag, window_action};
use oxproto::message::{
    Close, DisplayLayout, Error as ProtoError, FrameAck, FrameData, KeyEvent, ModifierSync, Output,
    PointerEvent, TextInput, WindowControl,
};
use oxproto::{close_reason, error_code, feature, Message};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::TlsConnector;

/// Every `n`th frame per window is printed, after the first. Otherwise a 30-60fps stream of
/// `RAW_BGRA` frames drowns the terminal in output within a second or two.
const FRAME_LOG_STRIDE: u64 = 60;

const USAGE: &str =
    "usage: oxclient <host:port> --pin <spki-hex> --token-file <path> [--name <client-name>] [--headless]";

/// Parsed command-line arguments.
#[derive(Debug, PartialEq, Eq)]
struct Cli {
    host: String,
    port: u16,
    pin: String,
    token_path: PathBuf,
    name: String,
    headless: bool,
}

/// Parse `argv[1..]` into a [`Cli`], or a human-readable error.
///
/// Deliberately hand-rolled instead of pulling in `clap`: this binary has a few flags and
/// keeping the dependency surface small matters for a bring-up tool that is meant to be the
/// simplest possible thing that proves the agent works.
fn parse_args(args: &[String]) -> Result<Cli, String> {
    let mut host_port: Option<String> = None;
    let mut pin: Option<String> = None;
    let mut token_path: Option<String> = None;
    let mut name: Option<String> = None;
    let mut headless = false;

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            // The token must never travel through argv: on any multi-user or even
            // single-user-but-multi-process Linux box, /proc/<pid>/cmdline is readable by
            // every process running as the same user (and by root), so a secret passed here
            // is effectively world-readable for the process lifetime. Reject it explicitly
            // rather than silently accepting a footgun.
            "--token" => {
                return Err(
                    "--token is not accepted: argv is readable by other processes on this \
                     machine (see /proc/<pid>/cmdline). Use --token-file <path> instead."
                        .to_string(),
                )
            }
            "--pin" => {
                pin = Some(iter.next().ok_or("--pin requires a value")?.clone());
            }
            "--token-file" => {
                token_path = Some(iter.next().ok_or("--token-file requires a value")?.clone());
            }
            "--name" => {
                name = Some(iter.next().ok_or("--name requires a value")?.clone());
            }
            "--headless" => {
                headless = true;
            }
            other if other.starts_with("--") => {
                return Err(format!("unknown argument: {other}"));
            }
            positional => {
                if host_port.is_some() {
                    return Err(format!("unexpected extra argument: {positional}"));
                }
                host_port = Some(positional.to_string());
            }
        }
    }

    let host_port = host_port.ok_or("missing <host:port>")?;
    let (host, port) = host_port
        .rsplit_once(':')
        .ok_or("<host:port> must contain a ':'")?;
    if host.is_empty() {
        return Err("<host:port> is missing a host".to_string());
    }
    let port: u16 = port
        .parse()
        .map_err(|_| format!("invalid port: {port:?}"))?;

    let pin = pin.ok_or("missing --pin <spki-hex>")?;
    let token_path = token_path.ok_or("missing --token-file <path>")?;

    Ok(Cli {
        host: host.to_string(),
        port,
        pin,
        token_path: PathBuf::from(token_path),
        name: name.unwrap_or_else(|| "oxclient".to_string()),
        headless,
    })
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let cli = match parse_args(&args) {
        Ok(cli) => cli,
        Err(err) => {
            eprintln!("oxclient: {err}");
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };

    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("oxclient: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let mut session = connect_session(&cli).await?;
    eprintln!(
        "oxclient: connected: protocol v{} session={:#x} codec={} features={:#x}",
        session.version, session.session_id, session.codec, session.features
    );

    // The agent must choose from what the client advertised. If it did not, every frame would
    // fail to decode; say so once, tell the agent why, and stop.
    if !decode::supports_codec(session.codec) {
        let message = format!(
            "agent selected codec {} which this client cannot decode (advertised {:?})",
            session.codec,
            decode::preferred_codecs()
        );
        session
            .send(&Message::Error(ProtoError {
                code: error_code::UNSUPPORTED_CODEC,
                message: message.clone(),
            }))
            .await?;
        session
            .send(&Message::Close(Close {
                reason: close_reason::ERROR,
            }))
            .await?;
        return Err(Box::new(io::Error::new(
            io::ErrorKind::InvalidData,
            message,
        )));
    }

    if cli.headless {
        run_headless(&mut session).await?;
    } else {
        run_windowed(session).await?;
    }

    Ok(())
}

async fn connect_session(
    cli: &Cli,
) -> Result<ClientSession<TlsStream<TcpStream>>, Box<dyn std::error::Error>> {
    // Never read from argv: load_token only ever sees a filesystem path.
    let auth_token = oxsec::load_token(&cli.token_path)?;
    let tls_config = oxsec::client_config_pinned(&cli.pin)?;

    let tcp = TcpStream::connect((cli.host.as_str(), cli.port)).await?;
    // The protocol's whole reason to exist is latency; Nagle's algorithm fights that on a
    // link that mixes small control/input messages with bulk frame data.
    tcp.set_nodelay(true)?;

    let connector = TlsConnector::from(tls_config);
    // The pin is what authenticates the peer (see OXPROTO.md §2); the certificate's name is
    // never checked, so any syntactically valid `ServerName` works here — the host the user
    // typed is as good as any other string for TLS's SNI extension.
    let server_name = ServerName::try_from(cli.host.clone())
        .map_err(|_| format!("{:?} is not a valid TLS server name", cli.host))?;
    let tls_stream = connector.connect(server_name, tcp).await?;

    let config = SessionConfig {
        auth_token,
        client_name: cli.name.clone(),
        // Descending preference, and only what this build can actually decode: with the `h264`
        // feature off this is `RAW_BGRA` alone, exactly what the bring-up client advertised.
        codecs: decode::preferred_codecs(),
        // The session handshake still advertises the same conservative synthetic output used by
        // the bring-up client. Native-window output enumeration can replace this after display
        // layout negotiation is treated as its own protocol surface.
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
    };

    Ok(ClientSession::connect(tls_stream, &config).await?)
}

async fn run_headless<S: AsyncRead + AsyncWrite + Unpin>(
    session: &mut ClientSession<S>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Per-window frame counters, so the rate-limit below is per window rather than global.
    let mut frame_counts: HashMap<u32, u64> = HashMap::new();
    let mut decoders = WindowDecoders::new();
    let clock = ClientClock::new();

    while let Some(event) = session.next_event().await? {
        print_event(&event, &mut frame_counts);
        match event {
            // Headless still decodes. It is the bring-up path for a machine with no display
            // server, so it has to exercise the decoder rather than route around it.
            ClientEvent::Frame(frame) => {
                let (window_id, frame_id) = (frame.window_id, frame.frame_id);
                let decoded_us = match decoders.decode(frame) {
                    Ok(_) => clock.now_us(),
                    Err(error) => {
                        eprintln!(
                            "oxclient: dropped frame {frame_id} for window {window_id}: {error}"
                        );
                        clock.now_us()
                    }
                };
                send_frame_ack(session, window_id, frame_id, decoded_us, clock.now_us()).await?;
            }
            ClientEvent::WindowClosed(closed) => decoders.forget(closed.window_id),
            _ => {}
        }
    }

    Ok(())
}

async fn run_windowed(
    session: ClientSession<TlsStream<TcpStream>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let (display_events_tx, display_events_rx) = mpsc::unbounded_channel();
    let (session_result_tx, session_result_rx) = oneshot::channel();

    // `winit` owns the main-thread event loop, while `ClientSession` is async network IO.
    // Run the session on the Tokio runtime and cross the thread boundary with winit's proxy
    // plus an mpsc channel for display-originated input and presentation events.
    oxdisplay::run(
        Box::new(CpuPresenter::new()),
        display_events_tx,
        move |commands| {
            tokio::spawn(async move {
                let result = run_session_task(session, commands, display_events_rx).await;
                let _ = session_result_tx.send(result.map_err(|error| error.to_string()));
            });
        },
    )?;

    match session_result_rx.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(Box::new(io::Error::other(error))),
        Err(_) => Err(Box::new(io::Error::other(
            "session task ended without reporting a result",
        ))),
    }
}

async fn run_session_task<S: AsyncRead + AsyncWrite + Unpin>(
    mut session: ClientSession<S>,
    commands: CommandSender,
    mut display_events: mpsc::UnboundedReceiver<DisplayEvent>,
) -> io::Result<()> {
    let mut model = WindowModel::new();
    let mut decoders = WindowDecoders::new();
    let mut geometry = GeometrySync::new();
    let clock = ClientClock::new();

    loop {
        tokio::select! {
            event = session.next_event() => {
                let event = match event {
                    Ok(Some(event)) => event,
                    Ok(None) => {
                        let _ = commands.send(DisplayCommand::Shutdown);
                        return Ok(());
                    }
                    Err(error) => {
                        let _ = commands.send(DisplayCommand::Shutdown);
                        return Err(error);
                    }
                };
                let closed = matches!(event, ClientEvent::Closed(_));
                for change in model.apply(event) {
                    // Every geometry change the client is about to cause itself — creating a
                    // native window, or applying the guest's own move — is recorded before the
                    // display layer acts on it, so the window manager events it provokes are
                    // recognised as echoes rather than as the user acting.
                    match &change {
                        ModelChange::Created(id) => {
                            if let Some(window) = model.get(*id) {
                                geometry.created(
                                    Instant::now(),
                                    *id,
                                    window.x,
                                    window.y,
                                    (window.width, window.height),
                                );
                            }
                        }
                        ModelChange::Moved(id) => {
                            if let Some(window) = model.get(*id) {
                                geometry.guest_moved(
                                    Instant::now(),
                                    *id,
                                    window.x,
                                    window.y,
                                    (window.width, window.height),
                                );
                            }
                        }
                        ModelChange::Destroyed(id) => {
                            decoders.forget(*id);
                            geometry.forget(*id);
                        }
                        _ => {}
                    }
                    let command = match change {
                        // Decode happens here rather than in the display layer so that what
                        // crosses to the winit thread is always presentable pixels.
                        //
                        // It is also synchronous on this task, which costs the input path up to
                        // one frame's decode time in added latency. That is the right trade at
                        // 800x600 and the wrong one at 4K; `Decoder` is `Send` precisely so this
                        // can move to its own thread without touching anything else.
                        ModelChange::Frame(frame) => {
                            let (window_id, frame_id) = (frame.window_id, frame.frame_id);
                            match decode_for_present(&mut decoders, frame) {
                                Some(decoded) => Some(DisplayCommand::Frame(decoded)),
                                None => {
                                    // A frame that will never be presented still has to be
                                    // acknowledged. The agent keeps a bounded number of
                                    // unacknowledged frames per window (OXPROTO.md §12) and the
                                    // display layer only reports frames it actually presented,
                                    // so a client joining mid-GOP would otherwise spend that
                                    // budget on frames it dropped and stall the window.
                                    let now = clock.now_us();
                                    if let Err(error) =
                                        send_frame_ack(&mut session, window_id, frame_id, now, now)
                                            .await
                                    {
                                        let _ = commands.send(DisplayCommand::Shutdown);
                                        return Err(error);
                                    }
                                    None
                                }
                            }
                        }
                        other => display_command_for_change(&model, other),
                    };
                    if let Some(command) = command {
                        if commands.send(command).is_err() {
                            return Ok(());
                        }
                    }
                }
                if closed {
                    return Ok(());
                }
            }
            event = display_events.recv() => {
                let Some(event) = event else {
                    return Ok(());
                };
                if let Err(error) =
                    handle_display_event(&mut session, &clock, &model, &mut geometry, event).await
                {
                    let _ = commands.send(DisplayCommand::Shutdown);
                    return Err(error);
                }
            }
        }
    }
}

/// Decodes one frame into presentable pixels, or `None` if there is nothing to show.
///
/// A decode failure is per frame and never ends the session: it is logged and the frame is
/// dropped, exactly like a frame the decoder legitimately swallows while waiting for a keyframe.
fn decode_for_present(decoders: &mut WindowDecoders, frame: FrameData) -> Option<FrameData> {
    let (window_id, frame_id) = (frame.window_id, frame.frame_id);
    match decoders.decode(frame) {
        Ok(decoded) => decoded,
        Err(error) => {
            eprintln!("oxclient: window {window_id} frame {frame_id} dropped: {error}");
            None
        }
    }
}

fn display_command_for_change(model: &WindowModel, change: ModelChange) -> Option<DisplayCommand> {
    Some(match change {
        ModelChange::Created(id) => DisplayCommand::CreateWindow(window_spec(model.get(id)?)),
        ModelChange::Destroyed(id) => DisplayCommand::DestroyWindow(id),
        ModelChange::Moved(id) => DisplayCommand::MoveWindow(window_spec(model.get(id)?)),
        ModelChange::Retitled(id) => DisplayCommand::RetitleWindow(window_spec(model.get(id)?)),
        ModelChange::StateChanged(id) => DisplayCommand::ChangeState(window_spec(model.get(id)?)),
        ModelChange::IconChanged(id) => DisplayCommand::ChangeIcon(window_spec(model.get(id)?)),
        ModelChange::Restacked => DisplayCommand::Restack(model.stack().to_vec()),
        // Frames never take this path: the caller runs them through the decoder, because a frame
        // that reaches the presenter still encoded is a black window, not an error.
        ModelChange::Frame(_) => return None,
        ModelChange::CursorShape(shape) => DisplayCommand::CursorShape(shape),
        ModelChange::CursorMoved { window_id, x, y } => {
            DisplayCommand::CursorPosition { window_id, x, y }
        }
        ModelChange::CursorVisibility(visible) => DisplayCommand::CursorVisibility(visible),
        ModelChange::AgentError { code, message } => DisplayCommand::AgentError { code, message },
        ModelChange::Closed => DisplayCommand::Shutdown,
    })
}

fn window_spec(window: &RemoteWindow) -> WindowSpec {
    WindowSpec {
        window_id: window.window_id,
        app_id: window.app_id.clone(),
        title: window.title.clone(),
        x: window.x,
        y: window.y,
        width: window.width,
        height: window.height,
        owner_id: window.owner_id,
        minimized: window.minimized,
        maximized: window.maximized,
        resizable: window.resizable,
        has_frame: window.has_frame,
        topmost: window.topmost,
        icon: window.icon.clone(),
    }
}

async fn handle_display_event<S: AsyncRead + AsyncWrite + Unpin>(
    session: &mut ClientSession<S>,
    clock: &ClientClock,
    model: &WindowModel,
    geometry: &mut GeometrySync,
    event: DisplayEvent,
) -> io::Result<()> {
    match event {
        DisplayEvent::Pointer {
            window_id,
            x,
            y,
            buttons,
            wheel_x,
            wheel_y,
        } => {
            session
                .send(&Message::PointerEvent(PointerEvent {
                    window_id,
                    x,
                    y,
                    buttons,
                    wheel_x,
                    wheel_y,
                    timestamp: clock.now_us(),
                }))
                .await?;
        }
        DisplayEvent::Key {
            scancode,
            pressed,
            extended,
        } => {
            let mut flags = 0;
            if pressed {
                flags |= key_flag::PRESSED;
            }
            if extended {
                flags |= key_flag::EXTENDED;
            }
            session
                .send(&Message::KeyEvent(KeyEvent {
                    scancode,
                    flags,
                    timestamp: clock.now_us(),
                }))
                .await?;
        }
        DisplayEvent::Text { text } if session.has_feature(feature::TEXT_INPUT) => {
            session
                .send(&Message::TextInput(TextInput { text }))
                .await?;
        }
        DisplayEvent::Text { .. } => {}
        DisplayEvent::Modifiers { modifiers, locks } => {
            session
                .send(&Message::ModifierSync(ModifierSync { modifiers, locks }))
                .await?;
        }
        DisplayEvent::Focused {
            window_id,
            focused: true,
        } if session.has_feature(feature::WINDOW_CONTROL) => {
            send_window_control(
                session,
                WindowControl {
                    window_id,
                    action: window_action::ACTIVATE,
                    x: 0,
                    y: 0,
                    width: 0,
                    height: 0,
                },
            )
            .await?;
        }
        DisplayEvent::Focused { .. } => {}
        DisplayEvent::CloseRequested { window_id }
            if session.has_feature(feature::WINDOW_CONTROL) =>
        {
            send_window_control(
                session,
                WindowControl {
                    window_id,
                    action: window_action::CLOSE,
                    x: 0,
                    y: 0,
                    width: 0,
                    height: 0,
                },
            )
            .await?;
        }
        DisplayEvent::CloseRequested { .. } => {}
        // The display layer reports what the host window manager did, which is not the same
        // thing as what the user asked for: a WM places and sizes every window the moment it is
        // created. `GeometrySync` is what tells the two apart, and what turns a host-screen
        // position into one that means something on the guest.
        DisplayEvent::ResizeRequested {
            window_id,
            width,
            height,
        } if session.has_feature(feature::WINDOW_CONTROL) => {
            if let Some((width, height)) =
                geometry.resized(Instant::now(), model, window_id, width, height)
            {
                send_window_control(
                    session,
                    WindowControl {
                        window_id,
                        action: window_action::RESIZE,
                        x: 0,
                        y: 0,
                        width,
                        height,
                    },
                )
                .await?;
            }
        }
        DisplayEvent::ResizeRequested { .. } => {}
        DisplayEvent::MoveRequested { window_id, x, y }
            if session.has_feature(feature::WINDOW_CONTROL) =>
        {
            if let Some((x, y)) = geometry.moved(Instant::now(), window_id, x, y) {
                send_window_control(
                    session,
                    WindowControl {
                        window_id,
                        action: window_action::MOVE,
                        x,
                        y,
                        width: 0,
                        height: 0,
                    },
                )
                .await?;
            }
        }
        DisplayEvent::MoveRequested { .. } => {}
        DisplayEvent::Minimized {
            window_id,
            minimized,
        } if session.has_feature(feature::WINDOW_CONTROL) => {
            send_window_control(
                session,
                WindowControl {
                    window_id,
                    action: if minimized {
                        window_action::MINIMIZE
                    } else {
                        window_action::RESTORE
                    },
                    x: 0,
                    y: 0,
                    width: 0,
                    height: 0,
                },
            )
            .await?;
        }
        DisplayEvent::Minimized { .. } => {}
        DisplayEvent::Presented {
            window_id,
            frame_id,
            decoded_us,
            presented_us,
        } => {
            send_frame_ack(session, window_id, frame_id, decoded_us, presented_us).await?;
        }
        DisplayEvent::BackendError { message } => {
            eprintln!("oxclient: display backend error: {message}");
        }
    }
    Ok(())
}

async fn send_window_control<S: AsyncRead + AsyncWrite + Unpin>(
    session: &mut ClientSession<S>,
    control: WindowControl,
) -> io::Result<()> {
    session.send(&Message::WindowControl(control)).await
}

async fn send_frame_ack<S: AsyncRead + AsyncWrite + Unpin>(
    session: &mut ClientSession<S>,
    window_id: u32,
    frame_id: u64,
    decoded_us: u64,
    presented_us: u64,
) -> io::Result<()> {
    if session.has_feature(feature::FRAME_ACK) {
        session
            .send(&Message::FrameAck(FrameAck {
                window_id,
                frame_id,
                decoded_us,
                presented_us,
            }))
            .await?;
    }
    Ok(())
}

fn print_event(event: &ClientEvent, frame_counts: &mut HashMap<u32, u64>) {
    match event {
        // Flags are printed by name rather than as a hex mask: this is the bring-up path, and
        // the one thing worth reading off it is whether the agent thinks a window's caption is
        // safe to crop — a judgement that is wrong for every DWM-frame-extended app if the
        // agent's heuristic is wrong, and invisible in a bare `flags=0x5`.
        ClientEvent::WindowOpened(w) => println!(
            "window opened: id={} app_id={} title={:?} geometry={}x{}+{}+{} flags=[{}]",
            w.window_id,
            w.app_id,
            w.title,
            w.width,
            w.height,
            w.x,
            w.y,
            describe_window_flags(w.flags)
        ),
        ClientEvent::Frame(f) => {
            let count = frame_counts.entry(f.window_id).or_insert(0);
            *count += 1;
            if *count == 1 || count.is_multiple_of(FRAME_LOG_STRIDE) {
                println!(
                    "frame: id={} window={} codec={} bytes={} keyframe={}",
                    f.frame_id,
                    f.window_id,
                    f.codec,
                    f.data.len(),
                    f.is_keyframe()
                );
            }
        }
        ClientEvent::WindowGeometry(g) => println!(
            "window geometry: id={} pos=({},{}) size={}x{}",
            g.window_id, g.x, g.y, g.width, g.height
        ),
        ClientEvent::WindowTitle(t) => {
            println!("window title: id={} title={:?}", t.window_id, t.title)
        }
        ClientEvent::WindowState(s) => println!(
            "window state: id={} state={} flags={:#x}",
            s.window_id, s.state, s.flags
        ),
        ClientEvent::WindowZOrder(z) => {
            println!(
                "window z-order: id={} above={}",
                z.window_id, z.above_window_id
            )
        }
        ClientEvent::WindowIcon(i) => println!(
            "window icon: id={} size={}x{} bytes={}",
            i.window_id,
            i.width,
            i.height,
            i.argb.len()
        ),
        ClientEvent::WindowClosed(c) => println!("window closed: id={}", c.window_id),
        ClientEvent::CursorShape(c) => println!(
            "cursor shape: id={} size={}x{} hotspot=({},{})",
            c.cursor_id, c.width, c.height, c.hotspot_x, c.hotspot_y
        ),
        ClientEvent::CursorPosition(p) => println!(
            "cursor position: window={} pos=({},{})",
            p.window_id, p.x, p.y
        ),
        ClientEvent::CursorVisibility(v) => {
            println!("cursor visibility: visible={}", v.visible)
        }
        ClientEvent::Error(e) => {
            eprintln!("oxclient: agent error {}: {}", e.code, e.message);
        }
        ClientEvent::Closed(c) => {
            eprintln!("oxclient: agent closed the session (reason={})", c.reason);
        }
    }
}

struct ClientClock {
    start: Instant,
}

impl ClientClock {
    fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }

    fn now_us(&self) -> u64 {
        self.start
            .elapsed()
            .as_micros()
            .try_into()
            .unwrap_or(u64::MAX)
    }
}

/// Render a `WindowOpened.flags` bitmask as the set of named flags it carries.
///
/// Unknown bits are reported as a residual hex value rather than dropped, so a client built
/// against an older protocol revision does not silently hide what a newer agent is saying.
fn describe_window_flags(flags: u32) -> String {
    use oxproto::message::window::window_flag;
    const NAMED: [(u32, &str); 5] = [
        (window_flag::RESIZABLE, "resizable"),
        (window_flag::HAS_FRAME, "has_frame"),
        (window_flag::TOPMOST, "topmost"),
        (window_flag::MINIMIZED, "minimized"),
        (window_flag::MAXIMIZED, "maximized"),
    ];
    let mut parts: Vec<String> = NAMED
        .iter()
        .filter(|(bit, _)| flags & bit != 0)
        .map(|(_, name)| (*name).to_string())
        .collect();
    let residual = flags & !NAMED.iter().fold(0, |acc, (bit, _)| acc | bit);
    if residual != 0 {
        parts.push(format!("unknown:{residual:#x}"));
    }
    if parts.is_empty() {
        "none".to_string()
    } else {
        parts.join(",")
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use oxclient::geometry::SETTLE;
    use oxproto::envelope::{channel, Reassembler};
    use oxproto::message::window::window_flag;
    use oxproto::message::{ServerHello, WindowOpened};
    use oxtransport::{read_message, write_message};
    use tokio::io::DuplexStream;

    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| s.to_string()).collect()
    }

    fn test_config() -> SessionConfig {
        SessionConfig {
            auth_token: "token".into(),
            client_name: "test".into(),
            codecs: decode::preferred_codecs(),
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
        }
    }

    /// A live session against an in-memory peer, with `WINDOW_CONTROL` negotiated.
    ///
    /// The returned stream is the agent's end: whatever the client sends can be read from it,
    /// which is the only way to assert on what the guest would actually be told to do.
    async fn connected_session() -> (ClientSession<DuplexStream>, DuplexStream, Reassembler) {
        let (client_io, mut server_io) = tokio::io::duplex(64 * 1024);
        let server = tokio::spawn(async move {
            let mut reassembler = Reassembler::new();
            let hello = read_message(&mut server_io, &mut reassembler)
                .await
                .expect("the client's hello arrives")
                .expect("and decodes");
            assert!(matches!(hello, Message::ClientHello(_)));
            write_message(
                &mut server_io,
                &Message::ServerHello(ServerHello {
                    version: 1,
                    features: feature::WINDOW_CONTROL,
                    session_id: 1,
                    codec: oxproto::codec::RAW_BGRA,
                }),
                channel::CONTROL,
            )
            .await
            .expect("the hello is written");
            (server_io, reassembler)
        });

        let session = ClientSession::connect(client_io, &test_config())
            .await
            .expect("the handshake completes");
        let (server_io, reassembler) = server.await.expect("the peer task finishes");
        assert!(session.has_feature(feature::WINDOW_CONTROL));
        (session, server_io, reassembler)
    }

    fn model_with(
        window_id: u32,
        x: i32,
        y: i32,
        width: u16,
        height: u16,
        flags: u32,
    ) -> WindowModel {
        let mut model = WindowModel::new();
        model.apply(ClientEvent::WindowOpened(WindowOpened {
            window_id,
            video_channel: channel::VIDEO_BASE,
            pid: 1,
            app_id: "app.exe".into(),
            title: "app".into(),
            x,
            y,
            width,
            height,
            dpi: 96,
            flags,
            owner_id: 0,
        }));
        model
    }

    /// A tracker for a window the host WM has already placed at `placed`, long enough ago that
    /// the settling window has closed by the time the code under test reads the clock.
    fn settled_geometry(
        window_id: u32,
        guest: (i32, i32),
        size: (u16, u16),
        placed: (i32, i32),
    ) -> GeometrySync {
        let past = Instant::now() - SETTLE - Duration::from_secs(1);
        let mut geometry = GeometrySync::new();
        geometry.created(past, window_id, guest.0, guest.1, size);
        assert_eq!(geometry.moved(past, window_id, placed.0, placed.1), None);
        geometry
    }

    /// Asks the client to close a window, which always produces a message.
    ///
    /// Used as a sentinel: if the next thing the agent reads is this close, nothing the test
    /// fed in beforehand was sent.
    async fn send_sentinel(
        session: &mut ClientSession<DuplexStream>,
        clock: &ClientClock,
        model: &WindowModel,
        geometry: &mut GeometrySync,
        window_id: u32,
    ) {
        handle_display_event(
            session,
            clock,
            model,
            geometry,
            DisplayEvent::CloseRequested { window_id },
        )
        .await
        .expect("the sentinel is sent");
    }

    async fn next_control(
        server_io: &mut DuplexStream,
        reassembler: &mut Reassembler,
    ) -> WindowControl {
        match read_message(server_io, reassembler)
            .await
            .expect("a message arrives")
            .expect("and decodes")
        {
            Message::WindowControl(control) => control,
            other => panic!("expected WindowControl, got {:#04x}", other.msg_type()),
        }
    }

    /// Connecting must not rearrange the guest. This is the regression: the host window manager
    /// places and sizes every native window as it is created, and forwarding those events moved
    /// four guest windows off a 1280x800 desktop and shrank one to 1x52.
    #[tokio::test]
    async fn a_freshly_created_window_sends_no_geometry_to_the_agent() {
        let (mut session, mut server_io, mut reassembler) = connected_session().await;
        let clock = ClientClock::new();
        let model = model_with(1, 100, 200, 800, 600, window_flag::RESIZABLE);
        let mut geometry = GeometrySync::new();
        geometry.created(Instant::now(), 1, 100, 200, (800, 600));

        // Exactly what the host WM did in the failure, replayed.
        for event in [
            DisplayEvent::MoveRequested {
                window_id: 1,
                x: 3257,
                y: 2262,
            },
            DisplayEvent::ResizeRequested {
                window_id: 1,
                width: 122,
                height: 47,
            },
        ] {
            handle_display_event(&mut session, &clock, &model, &mut geometry, event)
                .await
                .expect("the event is handled");
        }

        send_sentinel(&mut session, &clock, &model, &mut geometry, 1).await;
        assert_eq!(
            next_control(&mut server_io, &mut reassembler).await.action,
            window_action::CLOSE,
            "the WM's placement must not have reached the agent"
        );
    }

    #[tokio::test]
    async fn a_non_resizable_window_never_sends_a_resize() {
        let (mut session, mut server_io, mut reassembler) = connected_session().await;
        let clock = ClientClock::new();
        // charmap: a fixed-size dialog, which the failure shrank to 1x52.
        let model = model_with(1, 100, 200, 322, 197, window_flag::HAS_FRAME);
        let mut geometry = settled_geometry(1, (100, 200), (322, 197), (259, 2262));

        handle_display_event(
            &mut session,
            &clock,
            &model,
            &mut geometry,
            DisplayEvent::ResizeRequested {
                window_id: 1,
                width: 1,
                height: 52,
            },
        )
        .await
        .expect("the event is handled");

        send_sentinel(&mut session, &clock, &model, &mut geometry, 1).await;
        assert_eq!(
            next_control(&mut server_io, &mut reassembler).await.action,
            window_action::CLOSE,
            "a fixed-size window must never be asked to resize"
        );
    }

    #[tokio::test]
    async fn a_user_drag_sends_a_guest_space_move() {
        let (mut session, mut server_io, mut reassembler) = connected_session().await;
        let clock = ClientClock::new();
        let model = model_with(1, 100, 200, 800, 600, window_flag::RESIZABLE);
        // The window sits at guest (100,200) but at host (3257,2262) on a multi-monitor desktop.
        let mut geometry = settled_geometry(1, (100, 200), (800, 600), (3257, 2262));

        handle_display_event(
            &mut session,
            &clock,
            &model,
            &mut geometry,
            DisplayEvent::MoveRequested {
                window_id: 1,
                x: 3297,
                y: 2247,
            },
        )
        .await
        .expect("the event is handled");

        let control = next_control(&mut server_io, &mut reassembler).await;
        assert_eq!(control.action, window_action::MOVE);
        // The guest is told about the displacement applied to its own position, never about a
        // host screen coordinate it has no room for.
        assert_eq!((control.x, control.y), (140, 185));
    }

    #[tokio::test]
    async fn a_user_resize_of_a_resizable_window_reaches_the_agent() {
        let (mut session, mut server_io, mut reassembler) = connected_session().await;
        let clock = ClientClock::new();
        let model = model_with(1, 100, 200, 800, 600, window_flag::RESIZABLE);
        let mut geometry = settled_geometry(1, (100, 200), (800, 600), (100, 200));

        handle_display_event(
            &mut session,
            &clock,
            &model,
            &mut geometry,
            DisplayEvent::ResizeRequested {
                window_id: 1,
                width: 1024,
                height: 768,
            },
        )
        .await
        .expect("the event is handled");

        let control = next_control(&mut server_io, &mut reassembler).await;
        assert_eq!(control.action, window_action::RESIZE);
        assert_eq!((control.width, control.height), (1024, 768));
    }

    #[test]
    fn parses_a_valid_command_line() {
        let cli = parse_args(&args(&[
            "127.0.0.1:3390",
            "--pin",
            "ab12",
            "--token-file",
            "/etc/oxrdp/token",
            "--name",
            "my-laptop",
        ]))
        .expect("valid arguments should parse");

        assert_eq!(
            cli,
            Cli {
                host: "127.0.0.1".to_string(),
                port: 3390,
                pin: "ab12".to_string(),
                token_path: PathBuf::from("/etc/oxrdp/token"),
                name: "my-laptop".to_string(),
                headless: false,
            }
        );
    }

    #[test]
    fn defaults_the_client_name_when_omitted() {
        let cli = parse_args(&args(&[
            "host:1",
            "--pin",
            "ab12",
            "--token-file",
            "token.txt",
        ]))
        .expect("valid arguments should parse");

        assert_eq!(cli.name, "oxclient");
        assert!(!cli.headless);
    }

    #[test]
    fn parses_headless_flag() {
        let cli = parse_args(&args(&[
            "host:1",
            "--pin",
            "ab12",
            "--token-file",
            "token.txt",
            "--headless",
        ]))
        .expect("valid arguments should parse");

        assert!(cli.headless);
    }

    #[test]
    fn rejects_token_passed_on_the_command_line() {
        let err = parse_args(&args(&[
            "127.0.0.1:3390",
            "--pin",
            "ab12",
            "--token",
            "s3cret",
        ]))
        .expect_err("--token must be rejected");

        assert!(err.contains("--token"));
        assert!(err.contains("--token-file"));
    }

    #[test]
    fn rejects_missing_host_port() {
        let err = parse_args(&args(&["--pin", "ab12", "--token-file", "token.txt"]))
            .expect_err("missing positional argument must be rejected");
        assert!(err.contains("host:port"));
    }

    #[test]
    fn rejects_unknown_flags() {
        let err = parse_args(&args(&[
            "127.0.0.1:3390",
            "--pin",
            "ab12",
            "--token-file",
            "token.txt",
            "--bogus",
        ]))
        .expect_err("unknown flags must be rejected");
        assert!(err.contains("--bogus"));
    }

    #[test]
    fn rejects_host_port_without_a_colon() {
        let err = parse_args(&args(&[
            "no-colon-here",
            "--pin",
            "ab12",
            "--token-file",
            "token.txt",
        ]))
        .expect_err("a host:port without ':' must be rejected");
        assert!(err.contains(':'));
    }
}
