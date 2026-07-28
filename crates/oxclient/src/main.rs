//! `oxclient` — bring-up CLI: connect to an `oxagent` over pinned TLS and print the event
//! stream to stdout/stderr.
//!
//! This exists to verify the agent end to end (handshake, window lifecycle, frame delivery,
//! flow control) before any real rendering layer exists. It renders nothing: frames are
//! counted and acknowledged, never decoded or displayed.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use oxclient::session::{ClientEvent, ClientSession, SessionConfig};
use oxproto::message::{DisplayLayout, FrameAck, Output};
use oxproto::{codec, feature, Message};
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::TlsConnector;

/// Every `n`th frame per window is printed, after the first. Otherwise a 30-60fps stream of
/// `RAW_BGRA` frames drowns the terminal in output within a second or two.
const FRAME_LOG_STRIDE: u64 = 60;

const USAGE: &str =
    "usage: oxclient <host:port> --pin <spki-hex> --token-file <path> [--name <client-name>]";

/// Parsed command-line arguments.
#[derive(Debug, PartialEq, Eq)]
struct Cli {
    host: String,
    port: u16,
    pin: String,
    token_path: PathBuf,
    name: String,
}

/// Parse `argv[1..]` into a [`Cli`], or a human-readable error.
///
/// Deliberately hand-rolled instead of pulling in `clap`: this binary has four flags and
/// keeping the dependency surface small matters for a bring-up tool that is meant to be the
/// simplest possible thing that proves the agent works.
fn parse_args(args: &[String]) -> Result<Cli, String> {
    let mut host_port: Option<String> = None;
    let mut pin: Option<String> = None;
    let mut token_path: Option<String> = None;
    let mut name: Option<String> = None;

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
        client_name: cli.name,
        codecs: vec![codec::RAW_BGRA],
        // Placeholder: no real display backend exists yet (that is a separate milestone), so
        // this bring-up tool advertises a single synthetic 1920x1080 output at 1:1 scale and
        // 60 Hz. It is only used to let the agent complete DPI/geometry negotiation; nothing
        // is actually rendered onto it.
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

    let mut session = ClientSession::connect(tls_stream, &config).await?;
    eprintln!(
        "oxclient: connected: protocol v{} session={:#x} codec={} features={:#x}",
        session.version, session.session_id, session.codec, session.features
    );

    // Per-window frame counters, so the rate-limit below is per window rather than global.
    let mut frame_counts: HashMap<u32, u64> = HashMap::new();

    while let Some(event) = session.next_event().await? {
        match event {
            ClientEvent::WindowOpened(w) => println!(
                "window opened: id={} app_id={} title={:?} geometry={}x{}+{}+{}",
                w.window_id, w.app_id, w.title, w.width, w.height, w.x, w.y
            ),
            ClientEvent::Frame(f) => {
                let count = frame_counts.entry(f.window_id).or_insert(0);
                *count += 1;
                if *count == 1 || count.is_multiple_of(FRAME_LOG_STRIDE) {
                    println!(
                        "frame: id={} window={} bytes={} keyframe={}",
                        f.frame_id,
                        f.window_id,
                        f.data.len(),
                        f.is_keyframe()
                    );
                }

                // This is what lets the agent's flow control (OXPROTO.md §12) work at all: the
                // agent bounds how many frames it will have unacknowledged per window, and
                // without an ack it stalls after that many. decoded_us/presented_us are 0
                // because there is no decoder or renderer yet to time.
                if session.has_feature(feature::FRAME_ACK) {
                    session
                        .send(&Message::FrameAck(FrameAck {
                            window_id: f.window_id,
                            frame_id: f.frame_id,
                            decoded_us: 0,
                            presented_us: 0,
                        }))
                        .await?;
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
            ClientEvent::WindowZOrder(z) => println!(
                "window z-order: id={} above={}",
                z.window_id, z.above_window_id
            ),
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

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| s.to_string()).collect()
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
