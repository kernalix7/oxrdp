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
use std::time::{Duration, Instant};

use oxclient::clock::ClientClock;
use oxclient::decode::pipeline::{Backpressure, DecodePipeline, FrameReport, FrameSink};
use oxclient::decode::{self, WindowDecoders};
use oxclient::geometry::GeometrySync;
use oxclient::latency::LatencyMonitor;
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

/// How often the client pings the agent, per `OXPROTO.md` §15.
///
/// Every one of these is also a clock-offset sample, which is the only way an agent timestamp
/// can be compared with a client one.
const PING_INTERVAL: Duration = Duration::from_secs(1);

/// How often the latency report is printed, when it is enabled at all.
const LATENCY_REPORT_INTERVAL: Duration = Duration::from_secs(10);

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

/// Writes `log` records to stderr, alongside the binary's own `eprintln!` diagnostics.
///
/// `oxdisplay` reports through the `log` facade — dropped malformed frames, keys with no
/// scancode — and until this existed **nothing installed a logger**, so every one of those
/// records went to a no-op sink. A warning nobody can see is worse than no warning: it reads as
/// evidence that the path is quiet. Hand-rolled rather than pulling in `env_logger`, because all
/// that is wanted is a line on stderr.
struct StderrLogger;

static LOGGER: StderrLogger = StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            eprintln!(
                "oxclient: {}: {}: {}",
                record.level(),
                record.target(),
                record.args()
            );
        }
    }

    fn flush(&self) {}
}

/// A latency monitor, on only when `OXCLIENT_LATENCY` is set.
///
/// Off by default because the accounting keeps per-frame state and prints a block of numbers
/// nobody asked for; on, it is the only way to answer the question this project exists to
/// answer. What it measures and what it cannot see is documented in `oxclient::latency`.
fn latency_monitor() -> LatencyMonitor {
    match env::var("OXCLIENT_LATENCY") {
        Ok(value) if value != "0" && !value.eq_ignore_ascii_case("off") => {
            eprintln!(
                "oxclient: measuring capture-to-present latency; a report follows every {} seconds \
                 and once at exit",
                LATENCY_REPORT_INTERVAL.as_secs()
            );
            LatencyMonitor::enabled()
        }
        _ => LatencyMonitor::disabled(),
    }
}

/// Installs [`LOGGER`] at the level `OXCLIENT_LOG` asks for, warnings by default.
fn install_logger() {
    let level = match env::var("OXCLIENT_LOG").as_deref() {
        Ok("trace") => log::LevelFilter::Trace,
        Ok("debug") => log::LevelFilter::Debug,
        Ok("info") => log::LevelFilter::Info,
        Ok("error") => log::LevelFilter::Error,
        Ok("off") => log::LevelFilter::Off,
        _ => log::LevelFilter::Warn,
    };
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(level);
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    install_logger();
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

/// Everything the session task hands to the display thread.
///
/// A trait so the session task can be exercised without a window system: `CommandSender` is a
/// winit event-loop proxy, which cannot exist without a display server, and the behaviour worth
/// testing here — that input keeps flowing while decode is busy — has nothing to do with winit.
trait CommandSink: Clone + Send + 'static {
    /// Hand one command to the display thread. `false` means it is gone.
    fn send(&self, command: DisplayCommand) -> bool;
}

impl CommandSink for CommandSender {
    fn send(&self, command: DisplayCommand) -> bool {
        CommandSender::send(self, command).is_ok()
    }
}

/// Decoded frames go straight from a decode worker to the display thread.
///
/// The session task is deliberately not in this path: routing frames back through it would put
/// the pixel copy back on the task that also writes input events, which is the coupling this
/// whole arrangement exists to remove.
#[derive(Clone)]
struct DisplaySink<C: CommandSink>(C);

impl<C: CommandSink> FrameSink for DisplaySink<C> {
    fn deliver(&self, frame: FrameData) -> bool {
        self.0.send(DisplayCommand::Frame(frame))
    }
}

/// Resolves when a stalled window's decode queue has room again.
///
/// Cancel-safe, which is what lets it sit in a `select!` arm: `reserve` either yields a permit or
/// yields nothing, and dropping the permit hands the slot straight back. The caller is the only
/// producer on that queue, so the slot is still free when it sends.
async fn wait_for_decode_room(queue: Option<tokio::sync::mpsc::Sender<FrameData>>) -> bool {
    match queue {
        Some(queue) => queue.reserve().await.is_ok(),
        // Never selected: the branch is disabled whenever there is no stalled frame.
        None => std::future::pending().await,
    }
}

async fn run_session_task<S, C>(
    mut session: ClientSession<S>,
    commands: C,
    display_events: mpsc::UnboundedReceiver<DisplayEvent>,
) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
    C: CommandSink,
{
    let mut latency = latency_monitor();
    let result = run_session_loop(&mut session, commands, display_events, &mut latency).await;

    // Printed however the session ended, including on an error, because a session that fell over
    // is exactly the one whose numbers are worth reading.
    if latency.is_enabled() && latency.has_samples() {
        eprint!(
            "{}",
            latency.report(session.rtt_us().last(), session.clock_offset_us())
        );
    }
    result
}

async fn run_session_loop<S, C>(
    session: &mut ClientSession<S>,
    commands: C,
    mut display_events: mpsc::UnboundedReceiver<DisplayEvent>,
    latency: &mut LatencyMonitor,
) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
    C: CommandSink,
{
    let mut model = WindowModel::new();
    let mut geometry = GeometrySync::new();
    // The session's own clock, not a second one: every timestamp compared below has to share an
    // epoch with the ones the session puts on the wire.
    let clock = session.clock();

    // Decode runs on one worker thread per window, and decoded frames go from there straight to
    // the display thread. This task keeps only the protocol: reading the wire, writing input,
    // and acknowledging frames.
    let (dropped_tx, mut dropped_rx) = mpsc::unbounded_channel();
    let mut decoders = DecodePipeline::new(DisplaySink(commands.clone()), dropped_tx, clock);
    let mut pings = tokio::time::interval(PING_INTERVAL);
    let mut reports = tokio::time::interval(LATENCY_REPORT_INTERVAL);
    // A frame a worker had no room for. While one is held, this task stops reading the wire —
    // see `oxclient::decode::pipeline` for why the client must not simply drop it.
    let mut stalled: Option<Backpressure> = None;

    loop {
        // Cloned out before the `select!` so the waiting branch owns its queue handle and the
        // handler is free to take `stalled`.
        let stalled_queue = stalled.as_ref().map(|held| held.queue.clone());
        let is_stalled = stalled_queue.is_some();

        tokio::select! {
            event = session.next_event(), if !is_stalled => {
                let event = match event {
                    Ok(Some(event)) => event,
                    Ok(None) => {
                        commands.send(DisplayCommand::Shutdown);
                        return Ok(());
                    }
                    Err(error) => {
                        commands.send(DisplayCommand::Shutdown);
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
                                geometry.created(Instant::now(), *id, window.x, window.y);
                            }
                        }
                        ModelChange::Moved(id) => {
                            if let Some(window) = model.get(*id) {
                                geometry.guest_moved(Instant::now(), *id, window.x, window.y);
                            }
                        }
                        ModelChange::Destroyed(id) => {
                            decoders.forget(*id);
                            geometry.forget(*id);
                            latency.forget(*id);
                        }
                        _ => {}
                    }
                    let command = match change {
                        // Frames leave this task immediately. A worker decodes and hands the
                        // pixels to the display thread itself, so no frame's decode time is
                        // charged to the input path.
                        ModelChange::Frame(frame) => {
                            // Stamped here, the moment the frame is off the wire, because this
                            // is the boundary between "the network had it" and "we have it".
                            latency.on_arrival(
                                frame.window_id,
                                frame.frame_id,
                                frame.captured_us,
                                frame.encoded_us,
                                clock.now_us(),
                            );
                            if let Err(backpressure) = decoders.submit(frame) {
                                stalled = Some(backpressure);
                            }
                            None
                        }
                        other => display_command_for_change(&model, other),
                    };
                    if let Some(command) = command {
                        if !commands.send(command) {
                            return Ok(());
                        }
                    }
                }
                if closed {
                    return Ok(());
                }
            }
            // A worker freed a slot, so the frame this task was holding can go. Input has kept
            // flowing throughout: refusing to read the wire is what pushes back on the agent,
            // and it costs the input path nothing.
            alive = wait_for_decode_room(stalled_queue), if is_stalled => {
                if let Some(held) = stalled.take() {
                    if alive {
                        // Sole producer on that queue: the slot reserved above is still free.
                        let _ = held.queue.try_send(held.frame);
                    }
                }
            }
            // A frame that will never be presented still has to be acknowledged. The agent keeps
            // a bounded number of unacknowledged frames per window (OXPROTO.md §12) and the
            // display layer only reports frames it actually presented, so a client joining
            // mid-GOP would otherwise spend that budget on frames it dropped and stall the
            // window. The decision now happens on a worker, which is why this arrives by channel
            // rather than being decided here.
            report = dropped_rx.recv() => {
                let Some(report) = report else {
                    return Ok(());
                };
                match report {
                    // The only place the decode-completion time is known. The display thread
                    // sees the frame long after, so it cannot report this itself.
                    FrameReport::Decoded { window_id, frame_id, decoded_us } => {
                        latency.on_decoded(window_id, frame_id, decoded_us);
                    }
                    FrameReport::Dropped { window_id, frame_id, finished_us } => {
                        latency.on_dropped(window_id, frame_id);
                        if let Err(error) = send_frame_ack(
                            session, window_id, frame_id, finished_us, finished_us,
                        ).await {
                            commands.send(DisplayCommand::Shutdown);
                            return Err(error);
                        }
                    }
                }
            }
            // Liveness, and the only source of clock-offset samples. Until this existed the
            // client sent no pings at all, so agent timestamps could not be placed on the
            // client's clock and none of the accounting below was possible.
            _ = pings.tick() => {
                if let Err(error) = session.ping().await {
                    commands.send(DisplayCommand::Shutdown);
                    return Err(error);
                }
            }
            _ = reports.tick(), if latency.is_enabled() => {
                if latency.has_samples() {
                    // The *last* round trip, not the best one: the offset estimate comes from the
                    // most recent pong, so the error bound has to come from that same exchange.
                    // Quoting the minimum RTT beside an offset derived from a different, slower
                    // sample would claim a precision the estimate does not have.
                    eprint!(
                        "{}",
                        latency.report(session.rtt_us().last(), session.clock_offset_us())
                    );
                }
            }
            event = display_events.recv() => {
                let Some(event) = event else {
                    return Ok(());
                };
                if let Err(error) = handle_display_event(
                    session, &clock, &model, &mut geometry, latency, event,
                )
                .await
                {
                    commands.send(DisplayCommand::Shutdown);
                    return Err(error);
                }
            }
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
    latency: &mut LatencyMonitor,
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
            if let Some(offset) = session.clock_offset_us() {
                latency.on_presented(window_id, frame_id, presented_us, offset);
            }
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
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use oxclient::geometry::SETTLE;
    use oxproto::envelope::{channel, Reassembler};
    use oxproto::message::window::{frame_flag, window_flag};
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
        let session = connected_session_with(feature::WINDOW_CONTROL).await;
        assert!(session.0.has_feature(feature::WINDOW_CONTROL));
        session
    }

    /// As [`connected_session`], with the agent advertising exactly `features`.
    ///
    /// Feature-gated paths have to be tested from both sides of the gate, so the negotiated set
    /// is a parameter rather than a constant.
    async fn connected_session_with(
        features: u64,
    ) -> (ClientSession<DuplexStream>, DuplexStream, Reassembler) {
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
                    features,
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
    fn settled_geometry(window_id: u32, guest: (i32, i32), placed: (i32, i32)) -> GeometrySync {
        let past = Instant::now() - SETTLE - Duration::from_secs(1);
        let mut geometry = GeometrySync::new();
        geometry.created(past, window_id, guest.0, guest.1);
        // Placement while settling, then the first post-settling report that becomes the
        // anchor. Both silent; only a later report can be a gesture.
        assert_eq!(geometry.moved(past, window_id, placed.0, placed.1), None);
        assert_eq!(
            geometry.moved(
                past + SETTLE + Duration::from_millis(1),
                window_id,
                placed.0,
                placed.1
            ),
            None
        );
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
            &mut LatencyMonitor::disabled(),
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
        geometry.created(Instant::now(), 1, 100, 200);

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
            handle_display_event(
                &mut session,
                &clock,
                &model,
                &mut geometry,
                &mut LatencyMonitor::disabled(),
                event,
            )
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
        let mut geometry = settled_geometry(1, (100, 200), (259, 2262));

        handle_display_event(
            &mut session,
            &clock,
            &model,
            &mut geometry,
            &mut LatencyMonitor::disabled(),
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
        let mut geometry = settled_geometry(1, (100, 200), (3257, 2262));

        handle_display_event(
            &mut session,
            &clock,
            &model,
            &mut geometry,
            &mut LatencyMonitor::disabled(),
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

    /// Feeds one display event through the session and returns what reached the agent, or
    /// `None` if the client sent nothing at all.
    ///
    /// A close request is sent afterwards as a sentinel: it always produces a message, so if it
    /// is the first thing the agent reads then the event under test produced nothing.
    async fn wire_effect(features: u64, event: DisplayEvent) -> Option<Message> {
        let (mut session, mut server_io, mut reassembler) = connected_session_with(features).await;
        let clock = ClientClock::new();
        let model = model_with(7, 0, 0, 800, 600, window_flag::RESIZABLE);
        let mut geometry = GeometrySync::new();

        handle_display_event(
            &mut session,
            &clock,
            &model,
            &mut geometry,
            &mut LatencyMonitor::disabled(),
            event,
        )
        .await
        .expect("the event is handled");
        handle_display_event(
            &mut session,
            &clock,
            &model,
            &mut geometry,
            &mut LatencyMonitor::disabled(),
            DisplayEvent::CloseRequested { window_id: 7 },
        )
        .await
        .expect("the sentinel is sent");

        let first = read_message(&mut server_io, &mut reassembler)
            .await
            .expect("a message arrives")
            .expect("and decodes");
        match &first {
            Message::WindowControl(control) if control.action == window_action::CLOSE => None,
            _ => Some(first),
        }
    }

    /// A keystroke has to reach the agent, with a scancode that is not silently zero.
    ///
    /// Typing into the guest did not work end to end, and this is the half of the path that can
    /// be tested without a display server: everything from `DisplayEvent` to the wire.
    #[tokio::test]
    async fn a_key_press_reaches_the_agent_with_its_scancode() {
        let sent = wire_effect(
            feature::WINDOW_CONTROL,
            // 0x1e is `A` in PS/2 set 1.
            DisplayEvent::Key {
                scancode: 0x1e,
                pressed: true,
                extended: false,
            },
        )
        .await
        .expect("a key press must reach the agent");

        match sent {
            Message::KeyEvent(key) => {
                assert_eq!(key.scancode, 0x1e, "the scancode must survive intact");
                assert_ne!(
                    key.scancode, 0,
                    "a zero scancode types nothing on the guest"
                );
                assert_eq!(key.flags & key_flag::PRESSED, key_flag::PRESSED);
                assert_eq!(key.flags & key_flag::EXTENDED, 0);
            }
            other => panic!("expected KeyEvent, got {:#04x}", other.msg_type()),
        }
    }

    #[tokio::test]
    async fn a_key_release_and_the_extended_bit_survive() {
        let sent = wire_effect(
            feature::WINDOW_CONTROL,
            // Right-hand Ctrl: an E0-prefixed key, released.
            DisplayEvent::Key {
                scancode: 0x1d,
                pressed: false,
                extended: true,
            },
        )
        .await
        .expect("a key release must reach the agent");

        match sent {
            Message::KeyEvent(key) => {
                assert_eq!(key.scancode, 0x1d);
                assert_eq!(key.flags & key_flag::PRESSED, 0, "this is a release");
                assert_eq!(key.flags & key_flag::EXTENDED, key_flag::EXTENDED);
            }
            other => panic!("expected KeyEvent, got {:#04x}", other.msg_type()),
        }
    }

    #[tokio::test]
    async fn a_pointer_click_reaches_the_agent_window_relative() {
        let sent = wire_effect(
            feature::WINDOW_CONTROL,
            DisplayEvent::Pointer {
                window_id: 7,
                x: 475,
                y: 269,
                buttons: 1,
                wheel_x: 0,
                wheel_y: 3,
            },
        )
        .await
        .expect("a click must reach the agent");

        match sent {
            Message::PointerEvent(pointer) => {
                assert_eq!(pointer.window_id, 7, "addressed to the window under it");
                assert_eq!((pointer.x, pointer.y), (475, 269));
                assert_eq!(pointer.buttons, 1, "the button bit must be set");
                assert_eq!((pointer.wheel_x, pointer.wheel_y), (0, 3));
            }
            other => panic!("expected PointerEvent, got {:#04x}", other.msg_type()),
        }
    }

    #[tokio::test]
    async fn modifier_state_reaches_the_agent() {
        let sent = wire_effect(
            feature::WINDOW_CONTROL,
            DisplayEvent::Modifiers {
                modifiers: 0b0000_0101,
                locks: 0b0000_0010,
            },
        )
        .await
        .expect("modifier state must reach the agent");

        match sent {
            Message::ModifierSync(sync) => {
                assert_eq!(sync.modifiers, 0b0000_0101);
                assert_eq!(sync.locks, 0b0000_0010);
            }
            other => panic!("expected ModifierSync, got {:#04x}", other.msg_type()),
        }
    }

    #[tokio::test]
    async fn text_input_reaches_the_agent_only_when_negotiated() {
        let with = wire_effect(
            feature::WINDOW_CONTROL | feature::TEXT_INPUT,
            DisplayEvent::Text {
                text: "한글".to_string(),
            },
        )
        .await
        .expect("text must reach the agent when the feature is negotiated");

        match with {
            Message::TextInput(input) => assert_eq!(input.text, "한글"),
            other => panic!("expected TextInput, got {:#04x}", other.msg_type()),
        }

        // Without the feature the client must stay silent rather than send a message the agent
        // never agreed to handle.
        let without = wire_effect(
            feature::WINDOW_CONTROL,
            DisplayEvent::Text {
                text: "한글".to_string(),
            },
        )
        .await;
        assert!(without.is_none(), "text must not be sent unnegotiated");
    }

    /// Nothing could be measured across the two ends until this worked: the client sent no
    /// pings and discarded every pong, so an agent timestamp could not be placed on the client's
    /// clock at all.
    #[tokio::test]
    async fn a_ping_round_trip_produces_a_clock_estimate() {
        let (mut session, mut server_io, mut reassembler) = connected_session().await;
        assert_eq!(
            session.clock_offset_us(),
            None,
            "nothing is known before a pong"
        );

        session.ping().await.expect("the ping is sent");

        let ping = match read_message(&mut server_io, &mut reassembler)
            .await
            .expect("a message arrives")
            .expect("and decodes")
        {
            Message::Ping(ping) => ping,
            other => panic!("expected Ping, got {:#04x}", other.msg_type()),
        };

        // The agent answers with its own clock, which here reads a long way ahead of ours.
        let agent_us = ping.sent_us + 1_000_000;
        write_message(
            &mut server_io,
            &Message::Pong(oxproto::message::Pong {
                seq: ping.seq,
                sent_us: ping.sent_us,
                agent_us,
            }),
            channel::CONTROL,
        )
        .await
        .expect("the pong is written");

        // The pong is housekeeping, so it is consumed rather than surfaced. Reading one event
        // drives the session far enough to take it in; the peer then closes, ending the stream.
        drop(server_io);
        assert_eq!(session.next_event().await.expect("no error"), None);

        let offset = session.clock_offset_us().expect("a pong has landed");
        // The estimate assumes a symmetric path, so it lands within half the round trip of the
        // true offset. The round trip here is microseconds over an in-memory pipe.
        let rtt = session.rtt_us().last().expect("an rtt sample");
        let error = (offset - 1_000_000).unsigned_abs();
        assert!(
            error <= rtt / 2 + 1,
            "offset {offset} is further than half the {rtt} us round trip from the truth"
        );
    }

    /// `--headless` decodes inline, which is deliberate — it has no display and no input path to
    /// protect. What it must not lose by staying inline is the acknowledgement of a frame that
    /// produced no picture: §12's in-flight budget applies to it exactly as it does to the
    /// windowed path, and a frame swallowed silently would hold a slot in that budget forever.
    #[tokio::test]
    async fn headless_acknowledges_a_frame_it_could_not_decode() {
        let (client_io, mut server_io) = tokio::io::duplex(64 * 1024);

        let server = tokio::spawn(async move {
            let mut reassembler = Reassembler::new();
            let _ = read_message(&mut server_io, &mut reassembler).await;
            write_message(
                &mut server_io,
                &Message::ServerHello(ServerHello {
                    version: 1,
                    features: feature::FRAME_ACK,
                    session_id: 1,
                    codec: oxproto::codec::RAW_BGRA,
                }),
                channel::CONTROL,
            )
            .await
            .expect("hello");

            // A RAW_BGRA frame whose payload is too short for its geometry: the decoder rejects
            // it, so nothing is ever presented for it.
            let mut broken = raw_frame(1, 9);
            if let Message::FrameData(frame) = &mut broken {
                frame.data.truncate(3);
            }
            write_message(&mut server_io, &broken, channel::VIDEO_BASE)
                .await
                .expect("frame");

            read_message(&mut server_io, &mut reassembler)
                .await
                .expect("a message arrives")
                .expect("and decodes")
        });

        let mut session = ClientSession::connect(client_io, &test_config())
            .await
            .expect("handshake");
        // Run the loop alongside the peer rather than to completion: it ends only when the peer
        // closes, and the peer is what this test reads the acknowledgement from.
        let headless = tokio::spawn(async move {
            let _ = run_headless(&mut session).await;
        });

        let ack = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("an undecodable frame must still be acknowledged")
            .expect("peer");
        headless.abort();

        match ack {
            Message::FrameAck(ack) => assert_eq!((ack.window_id, ack.frame_id), (1, 9)),
            other => panic!("expected FrameAck, got {:#04x}", other.msg_type()),
        }
    }

    /// A display thread that can be frozen mid-frame.
    ///
    /// Blocking in `send` is exactly what a worker busy decoding looks like from the session
    /// task: the frame is somewhere else, and this task is not waiting on it.
    #[derive(Clone)]
    struct GatedCommands {
        commands: std::sync::mpsc::Sender<DisplayCommand>,
        gate: Arc<Mutex<Option<std::sync::mpsc::Receiver<()>>>>,
    }

    impl CommandSink for GatedCommands {
        fn send(&self, command: DisplayCommand) -> bool {
            if matches!(command, DisplayCommand::Frame(_)) {
                if let Some(gate) = self.gate.lock().expect("the gate is not poisoned").as_ref() {
                    let _ = gate.recv();
                }
            }
            self.commands.send(command).is_ok()
        }
    }

    fn raw_frame(window_id: u32, frame_id: u64) -> Message {
        Message::FrameData(oxproto::message::FrameData {
            window_id,
            frame_id,
            codec: oxproto::codec::RAW_BGRA,
            flags: frame_flag::KEYFRAME,
            width: 2,
            height: 2,
            captured_us: frame_id,
            encoded_us: frame_id,
            data: vec![0x20; 2 * 2 * 4],
        })
    }

    /// The reason this whole arrangement exists: a frame being decoded must not delay input.
    ///
    /// Decode is frozen here for the duration, and every queue between the wire and the decoder
    /// is filled, so the session task is holding a frame it cannot place — the worst case the
    /// design has to survive. An input event fed in that state must still reach the agent. When
    /// decode ran on the session task this could not have held: the task would have been inside
    /// the decoder, and the input branch of its `select!` could not run until it returned.
    #[tokio::test]
    async fn input_reaches_the_agent_while_decode_is_blocked() {
        let (client_io, mut server_io) = tokio::io::duplex(1024 * 1024);
        let (release, gate) = std::sync::mpsc::channel();
        let (display_tx, _display_rx) = std::sync::mpsc::channel();
        let (events_tx, events_rx) = mpsc::unbounded_channel();

        let server = tokio::spawn(async move {
            let mut reassembler = Reassembler::new();
            let _ = read_message(&mut server_io, &mut reassembler).await;
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
            .expect("hello");

            write_message(
                &mut server_io,
                &Message::WindowOpened(WindowOpened {
                    window_id: 1,
                    video_channel: channel::VIDEO_BASE,
                    pid: 1,
                    app_id: "app.exe".into(),
                    title: "app".into(),
                    x: 0,
                    y: 0,
                    width: 2,
                    height: 2,
                    dpi: 96,
                    flags: window_flag::RESIZABLE,
                    owner_id: 0,
                }),
                channel::CONTROL,
            )
            .await
            .expect("window");

            // Comfortably more frames than the worker's queue holds, so the session task ends up
            // holding one it cannot place.
            for frame_id in 0..16 {
                write_message(&mut server_io, &raw_frame(1, frame_id), channel::VIDEO_BASE)
                    .await
                    .expect("frame");
            }
            (server_io, reassembler)
        });

        let session = ClientSession::connect(client_io, &test_config())
            .await
            .expect("handshake");
        let (mut server_io, mut reassembler) = server.await.expect("peer");

        let commands = GatedCommands {
            commands: display_tx,
            gate: Arc::new(Mutex::new(Some(gate))),
        };
        let task = tokio::spawn(run_session_task(session, commands, events_rx));

        // Give the frames time to arrive and jam the pipeline before input is offered, so the
        // test is exercising the stalled state rather than racing it.
        tokio::time::sleep(Duration::from_millis(150)).await;

        events_tx
            .send(DisplayEvent::Pointer {
                window_id: 1,
                x: 42,
                y: 24,
                buttons: 1,
                wheel_x: 0,
                wheel_y: 0,
            })
            .expect("the session task is listening");

        // The pointer event must arrive while decode is still frozen. Housekeeping is skipped
        // rather than asserted against: the session pings on connect and every second after, and
        // this test is about input not queueing behind decode. Without the timeout a regression
        // here would hang rather than fail.
        let pointer = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let message = read_message(&mut server_io, &mut reassembler)
                    .await
                    .expect("a message arrives")
                    .expect("and decodes");
                if !matches!(message, Message::Ping(_) | Message::Pong(_)) {
                    return message;
                }
            }
        })
        .await
        .expect("input must not wait for decode");

        match pointer {
            Message::PointerEvent(event) => {
                assert_eq!((event.x, event.y), (42, 24));
                assert_eq!(event.buttons, 1);
            }
            other => panic!("expected PointerEvent, got {:#04x}", other.msg_type()),
        }

        // Unfreeze so the task can finish rather than leaking a blocked worker.
        for _ in 0..32 {
            let _ = release.send(());
        }
        task.abort();
    }

    #[tokio::test]
    async fn a_user_resize_of_a_resizable_window_reaches_the_agent() {
        let (mut session, mut server_io, mut reassembler) = connected_session().await;
        let clock = ClientClock::new();
        let model = model_with(1, 100, 200, 800, 600, window_flag::RESIZABLE);
        let mut geometry = settled_geometry(1, (100, 200), (100, 200));
        // A resize needs a post-settling baseline before a later, different size is intent.
        assert_eq!(
            geometry.resized(Instant::now(), &model, 1, 800, 600),
            None,
            "the first size after settling is a baseline"
        );

        handle_display_event(
            &mut session,
            &clock,
            &model,
            &mut geometry,
            &mut LatencyMonitor::disabled(),
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
