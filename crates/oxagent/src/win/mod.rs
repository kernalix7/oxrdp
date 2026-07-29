//! Windows-only agent internals: window enumeration and per-window capture.
#![cfg(windows)]

mod appid;
pub mod capture;
mod encode;
pub mod enumerate;
mod input;

use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use oxsec::AgentIdentity;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio_rustls::TlsAcceptor;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::RemoteDesktop::{ProcessIdToSessionId, WTSGetActiveConsoleSessionId};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};

use crate::config::AgentConfig;
use crate::handshake::RAW_BGRA_ONLY;
use crate::serve::{run_session, SessionParams};
use capture::WindowCapture;
use encode::{probe_h264_support, EncoderKind, WinFrameEncoder};
use enumerate::enumerate_windows;
use input::WinInputSink;

/// Deadline for the *whole* pre-authentication phase: TLS accept and the `ClientHello` read,
/// together (`crate::serve::run_session`'s `pre_auth_deadline`) — not two independent timeouts,
/// which a slow trickle across their boundary would defeat. 20 seconds is generous for a real
/// client on a bad link (TLS is a handful of round trips; `ClientHello` is one small message)
/// and gives an attacker nothing: holding a socket open for 20s costs it as much as holding one
/// open forever, for zero benefit past that point.
const PRE_AUTH_DEADLINE: Duration = Duration::from_secs(20);

/// How many connections may be mid-handshake (TLS accept through `ClientHello`) at once.
/// Spawning a task per connection fixes "one silent peer blocks the listener forever" (see
/// `run_agent`), but without a cap on *concurrent* pre-auth attempts, the same attacker just
/// moves the target from "one socket blocks everything" to "ten thousand sockets exhaust
/// memory". A handful of legitimate simultaneous connection attempts never needs anywhere near
/// this many at once.
const PRE_AUTH_CONCURRENCY: usize = 4;

/// Opt into per-monitor DPI awareness.
///
/// Must happen before any window geometry is read: without it Windows lies to the process
/// about window rectangles on a scaled display (returning virtualized, scaled-down
/// coordinates), which would misalign every captured window and every input coordinate.
fn set_dpi_awareness() {
    // SAFETY: no arguments beyond a well-known context constant; failure is non-fatal.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// Log which Windows session this process is running in, and whether it is the one with the
/// interactive desktop.
///
/// `SendInput` only ever reaches windows in the calling process's own session — there is no
/// cross-session variant. If this process's session does not match the active console session,
/// every keyboard/mouse injection this agent will ever attempt is doomed before it even calls
/// `SendInput`, and this is exactly the shape of setup that produces that: a guest agent
/// installed as a Windows service runs in Session 0 by default (Session 0 Isolation, since
/// Vista), completely separate from the interactive user's session, with no visible desktop of
/// its own to inject into. Purely informational — a mismatch does not stop the agent from
/// starting, since it affects only input, not capture or anything else this process does.
fn log_session_context() {
    // SAFETY: takes no arguments and cannot fail.
    let pid = unsafe { GetCurrentProcessId() };
    let mut own_session = 0u32;
    // SAFETY: `pid` is this process's own id, valid for the lifetime of this call;
    // `own_session` is a local out-parameter.
    let resolved = unsafe { ProcessIdToSessionId(pid, &mut own_session) }.is_ok();
    // SAFETY: takes no arguments and cannot fail.
    let active_session = unsafe { WTSGetActiveConsoleSessionId() };
    if !resolved {
        eprintln!("oxagent: session: could not resolve this process's session id");
    } else if own_session == active_session {
        eprintln!("oxagent: session: running in session {own_session}, the active console session");
    } else {
        eprintln!(
            "oxagent: session: running in session {own_session}, but the active console session is {active_session} — synthetic keyboard/mouse input cannot reach a different session's desktop"
        );
    }
}

/// Agent entry point: set up TLS, bind the listener, and serve clients until shutdown.
///
/// Each connection is authenticated before the capture source is touched (see
/// [`crate::serve::run_session`]). Connections are accepted and handshaked *concurrently* —
/// each on its own task — but only one authenticated session drives windows at a time: a second
/// client would contend for the same windows, and multi-session support is out of scope for v0.
/// See [`PRE_AUTH_DEADLINE`] and [`PRE_AUTH_CONCURRENCY`] for why the concurrent part exists at
/// all: a sequential accept loop with no timeout let one silent connection block the listener,
/// and everyone after it, forever.
pub fn run_agent(config: &AgentConfig, print_pin: bool) -> ExitCode {
    set_dpi_awareness();
    log_session_context();

    let identity =
        match AgentIdentity::load_or_generate(&config.cert_path, &config.key_path, "oxagent") {
            Ok(id) => id,
            Err(e) => {
                eprintln!("oxagent: TLS identity: {e}");
                return ExitCode::from(2);
            }
        };
    let pin = match identity.spki_pin() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("oxagent: certificate pin: {e}");
            return ExitCode::from(2);
        }
    };
    if print_pin {
        println!("{pin}");
        return ExitCode::SUCCESS;
    }

    let token = match oxsec::load_token(&config.token_path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("oxagent: {}: {e}", config.token_path.display());
            return ExitCode::from(2);
        }
    };
    let tls_config = match oxsec::server_config(&identity) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("oxagent: TLS server config: {e}");
            return ExitCode::from(2);
        }
    };

    if !capture::is_supported() {
        eprintln!("oxagent: Windows.Graphics.Capture is not available on this build of Windows");
        return ExitCode::from(1);
    }

    // Probed once, here, rather than per-session: hardware H.264 support is a fact about this
    // guest for the lifetime of this process, not something that changes per connection. Report
    // which kind (or none) was found now — "report it once, at session start" (P5a) is most
    // useful the moment anyone can see it, and for a freshly-started agent that is before the
    // first session can even connect.
    let h264_kind = probe_h264_support();
    let supported_codecs: std::sync::Arc<[u8]> = match h264_kind {
        Some(kind) => {
            eprintln!("oxagent: H.264 encode: {kind}");
            std::sync::Arc::from([oxproto::codec::RAW_BGRA, oxproto::codec::H264])
        }
        None => {
            eprintln!("oxagent: H.264 encode: unavailable, RAW_BGRA only");
            std::sync::Arc::from(RAW_BGRA_ONLY)
        }
    };

    eprintln!("oxagent: listening on {} (pin {pin})", config.bind);

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("oxagent: runtime: {e}");
            return ExitCode::from(1);
        }
    };
    runtime.block_on(async move {
        let acceptor = TlsAcceptor::from(tls_config);
        let listener = match TcpListener::bind(config.bind).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("oxagent: bind {}: {e}", config.bind);
                return ExitCode::from(1);
            }
        };

        let token: Arc<str> = Arc::from(token);
        // Exactly one authenticated session drives windows at a time — unchanged from before
        // this loop grew concurrency, just now enforced explicitly (`crate::serve::run_session`
        // checks it) instead of holding by construction from a strictly sequential loop.
        let session_slot = Arc::new(Semaphore::new(1));
        let pre_auth_slots = Arc::new(Semaphore::new(PRE_AUTH_CONCURRENCY));

        // `WinWindowSource`/`WinInputSink` hold WinRT/D3D11 COM interfaces, and windows-rs
        // deliberately leaves those `!Send` (every one wraps a `NonNull<c_void>`, which itself
        // opts out of the auto trait) rather than asserting a cross-thread safety guarantee
        // Windows does not make for arbitrary COM objects. `tokio::spawn` requires `Send` and
        // cannot hold one across an `.await`, so per-connection tasks below use a `LocalSet`
        // and `spawn_local` instead: local tasks still interleave freely — `accept()` still
        // returns immediately, several connections still authenticate concurrently — they are
        // just guaranteed to stay on the one thread already driving this `block_on` call, which
        // is exactly where these COM objects were already confined before this file had any
        // concurrency at all.
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async move {
                let mut session_id: u64 = 1;
                loop {
                    // `listener.accept()` itself is not the denial-of-service vector: it
                    // completes as soon as a peer's TCP handshake lands, regardless of what
                    // that peer does next. The vector is everything after — TLS accept and the
                    // `ClientHello` read — which is why only that part moves into a spawned
                    // task with its own deadline below.
                    let (tcp, peer) = match listener.accept().await {
                        Ok(pair) => pair,
                        Err(e) => {
                            eprintln!("oxagent: accept: {e}");
                            continue;
                        }
                    };

                    // Reject before spending a TLS accept's worth of work on a connection that
                    // has nowhere to go: with `PRE_AUTH_CONCURRENCY` already saturated,
                    // spawning this one anyway would just trade "one socket blocks the
                    // listener" for "unbounded sockets exhaust memory" — the same failure mode
                    // in a different shape.
                    let Ok(pre_auth_permit) = Arc::clone(&pre_auth_slots).try_acquire_owned()
                    else {
                        eprintln!("oxagent: too many connections mid-handshake; dropping {peer}");
                        continue;
                    };

                    let acceptor = acceptor.clone();
                    let token = Arc::clone(&token);
                    let session_slot = Arc::clone(&session_slot);
                    let supported_codecs = Arc::clone(&supported_codecs);
                    let params = SessionParams {
                        target_fps: config.target_fps,
                        max_frames_in_flight: config.max_frames_in_flight,
                    };
                    let id = session_id;
                    session_id += 1;

                    // One task per connection: accept must return to the loop immediately, or a
                    // single stalled peer blocks every legitimate connection behind it — the
                    // denial of service this whole restructure exists to close. It also bounds
                    // the blast radius of a panic anywhere in TLS, the handshake, the drive
                    // loop or input injection to this one task rather than the whole agent
                    // process, which today is awaited inline.
                    tokio::task::spawn_local(async move {
                        // Held for the rest of this task and released on drop, covering TLS
                        // accept and (via `deadline` threaded into `run_session`) the
                        // `ClientHello` read too — exactly the span `PRE_AUTH_CONCURRENCY`
                        // needs to bound.
                        let _pre_auth_permit = pre_auth_permit;

                        let deadline = Instant::now() + PRE_AUTH_DEADLINE;
                        let tls = match tokio::time::timeout_at(
                            tokio::time::Instant::from(deadline),
                            acceptor.accept(tcp),
                        )
                        .await
                        {
                            Ok(Ok(tls)) => tls,
                            Ok(Err(e)) => {
                                // A failed TLS handshake is routine (a port scan, a client with
                                // the wrong pin); log and move on.
                                eprintln!("oxagent: TLS handshake from {peer} failed: {e}");
                                return;
                            }
                            Err(_elapsed) => {
                                eprintln!("oxagent: TLS handshake from {peer} timed out");
                                return;
                            }
                        };

                        eprintln!("oxagent: session {id} from {peer}");
                        let mut source = WinWindowSource::new();
                        let mut sink = WinInputSink::new();
                        // Constructed unconditionally, even when H.264 is unavailable this run
                        // (`h264_kind` is `None`): building one costs nothing (no Media
                        // Foundation call happens until `submit`, which only ever runs when the
                        // negotiated codec is `H264` — impossible unless `supported_codecs`
                        // offered it), and it keeps `run_session`'s generic `E: FrameEncoder`
                        // satisfied without a second code path for "no encoder at all".
                        let mut encoder = WinFrameEncoder::new(
                            h264_kind.unwrap_or(EncoderKind::Software),
                            params.target_fps,
                        );
                        match run_session(
                            tls,
                            &mut source,
                            &mut sink,
                            &mut encoder,
                            params,
                            id,
                            &token,
                            &supported_codecs,
                            deadline,
                            &session_slot,
                        )
                        .await
                        {
                            Ok(negotiated) => eprintln!(
                                "oxagent: session {id} with '{}' ended",
                                negotiated.client_name
                            ),
                            Err(e) => eprintln!("oxagent: session {id}: {e}"),
                        }
                    });
                }
            })
            .await
    })
}

/// The Windows implementation of the session driver's platform interface.
///
/// Holds one live [`WindowCapture`] per window the driver has asked about, created lazily on
/// first request and dropped when the window disappears.
pub struct WinWindowSource {
    captures: std::collections::HashMap<isize, WindowCapture>,
}

impl WinWindowSource {
    /// A source with no captures started.
    pub fn new() -> Self {
        Self {
            captures: std::collections::HashMap::new(),
        }
    }
}

impl Default for WinWindowSource {
    fn default() -> Self {
        Self::new()
    }
}

impl crate::serve::WindowSource for WinWindowSource {
    fn live_windows(&mut self) -> Vec<crate::serve::SourceWindow> {
        let live = enumerate_windows();
        // Drop captures for windows that are gone, so their D3D resources are released.
        let alive: std::collections::HashSet<isize> = live.iter().map(|w| w.hwnd).collect();
        self.captures.retain(|handle, _| alive.contains(handle));

        live.into_iter()
            // Minimized windows are *not* filtered out here. They used to be, on the theory
            // that a minimized window "has nothing to capture" — but `live_windows()` is also
            // what the registry diffs to decide a window closed (`WindowRegistry::retain_live`
            // in `crate::serve`), so dropping a minimized window from this list made the driver
            // report it as closed, and then re-open it as a brand-new window on restore, losing
            // its position, size, stacking and identity from the client's point of view. The
            // window stays tracked and reported; `crate::serve::pump_frames` is what actually
            // stops capturing it while minimized, based on the `MINIMIZED` show state carried
            // in `WindowOpened`/`WindowState`, not by making it disappear here.
            .map(|w| {
                let identity = appid::identify(HWND(w.hwnd as *mut core::ffi::c_void));
                crate::serve::SourceWindow {
                    handle: w.hwnd,
                    pid: identity.as_ref().map_or(0, |i| i.pid),
                    app_id: identity.map_or_else(|| "windows-app".to_string(), |i| i.app_id),
                    title: w.title,
                    x: w.x,
                    y: w.y,
                    width: w.width,
                    height: w.height,
                    // TODO(P4): report the window's real per-monitor DPI.
                    dpi: 96,
                    minimized: w.minimized,
                    maximized: w.maximized,
                    resizable: w.resizable,
                    has_frame: w.has_frame,
                    topmost: w.topmost,
                }
            })
            .collect()
    }

    fn next_frame(&mut self, handle: isize) -> Option<crate::serve::SourceFrame> {
        let capture = match self.captures.entry(handle) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                let hwnd = HWND(handle as *mut core::ffi::c_void);
                match WindowCapture::new(hwnd) {
                    Ok(c) => e.insert(c),
                    Err(err) => {
                        eprintln!("oxagent: capture failed for {handle:#x}: {err}");
                        return None;
                    }
                }
            }
        };

        match capture.try_next_frame() {
            Ok(Some(frame)) => Some(crate::serve::SourceFrame {
                width: frame.width.min(u32::from(u16::MAX)) as u16,
                height: frame.height.min(u32::from(u16::MAX)) as u16,
                data: frame.bgra,
            }),
            Ok(None) => None,
            Err(err) => {
                eprintln!("oxagent: capture error for {handle:#x}: {err}");
                // Drop the broken capture so the next tick recreates it.
                self.captures.remove(&handle);
                None
            }
        }
    }
}
