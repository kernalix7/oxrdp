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
use windows::core::PWSTR;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::RemoteDesktop::{
    ProcessIdToSessionId, WTSActive, WTSConnectQuery, WTSConnectState, WTSConnected,
    WTSDisconnected, WTSDown, WTSFreeMemory, WTSGetActiveConsoleSessionId, WTSIdle, WTSInit,
    WTSListen, WTSQuerySessionInformationW, WTSReset, WTSShadow, WTS_CONNECTSTATE_CLASS,
    WTS_CURRENT_SERVER_HANDLE,
};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};

use crate::config::AgentConfig;
use crate::handshake::RAW_BGRA_ONLY;
use crate::serve::{run_session, CaptureIntent, SessionParams};
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

/// Log which Windows session this process is running in, and whether that session's desktop can
/// actually receive injected input.
///
/// `SendInput` only ever reaches windows in the calling process's own session — there is no
/// cross-session variant — so the question that matters is "does *this* session have an
/// interactive desktop attached", not "is this session the one on the physical console"
/// (`WTSGetActiveConsoleSessionId`, this function's previous check). Those two questions have the
/// same answer only on a machine nobody has ever remoted into. In this project's own topology
/// they do not: dockur's autologon owns session 1, and an RDP client reconnecting as the same
/// user *takes over* session 1 rather than creating a new one, leaving a fresh, unused console
/// session behind. A process does not move sessions, so an agent already running in session 1
/// stays there — in the session the user is actually driving — while `WTSGetActiveConsoleSessionId`
/// now reports the different, idle session left behind. Comparing against the console session
/// flagged exactly that as broken; it was the working case the whole time.
///
/// The right question is `WTSQuerySessionInformation(WTSConnectState)` on this process's own
/// session (see [`session_connect_state`]): [`WTSActive`] — "a user is logged on... actively
/// connected to the device" — is the one state input can reach, on the console or over RDP
/// alike, and it says so directly instead of by comparing against a session id that means
/// something else. Every other state means there is no live desktop on the other end of
/// `SendInput` — most usefully [`WTSDisconnected`], which per its own documentation covers not
/// only a disconnected RDP session but a *locked* one ("such as when the user has chosen to exit
/// to the lock screen"), and Session 0, which never reports `Active` because no interactive user
/// is ever logged onto it — exactly the case of a guest agent installed as a true Windows service
/// (Session 0 Isolation, since Vista).
///
/// Purely informational either way — a bad state does not stop the agent from starting, since it
/// affects only input, not capture or anything else this process does.
fn log_session_context() {
    // SAFETY: takes no arguments and cannot fail.
    let pid = unsafe { GetCurrentProcessId() };
    let mut own_session = 0u32;
    // SAFETY: `pid` is this process's own id, valid for the lifetime of this call;
    // `own_session` is a local out-parameter.
    let resolved = unsafe { ProcessIdToSessionId(pid, &mut own_session) }.is_ok();
    if !resolved {
        eprintln!("oxagent: session: could not resolve this process's session id");
        return;
    }

    // `WTS_CONNECTSTATE_CLASS`'s variants (`WTSActive`, `WTSDisconnected`, ...) are named after
    // the Win32 API, not Rust's `UPPER_SNAKE_CASE` convention for constants, so matching them as
    // bare pattern identifiers trips `non_upper_case_globals` — rustc's guard against a pattern
    // that looks like it binds a fresh variable but is actually comparing against a constant.
    // Comparing with `==` sidesteps that entirely and is no less clear.
    let Some(state) = session_connect_state(own_session) else {
        eprintln!(
            "oxagent: session: could not query connect state for session {own_session}; input \
             health is unknown"
        );
        return;
    };

    if state == WTSActive {
        // Still worth a note when this differs from the console session: that is exactly the
        // RDP-takeover topology this function's doc explains, not a problem, and an operator
        // staring at "session 1, but session 3 is the console" deserves that context rather
        // than silence about it.
        // SAFETY: takes no arguments and cannot fail.
        let active_session = unsafe { WTSGetActiveConsoleSessionId() };
        if own_session == active_session {
            eprintln!(
                "oxagent: session: running in session {own_session}, WTSConnectState=Active — \
                 this is also the active console session"
            );
        } else {
            eprintln!(
                "oxagent: session: running in session {own_session}, WTSConnectState=Active — \
                 input can reach this session's desktop (the active console session is \
                 {active_session} instead; that differs whenever a client has taken this \
                 session over via RDP, which is expected here, not broken)"
            );
        }
    } else {
        eprintln!(
            "oxagent: session: running in session {own_session}, WTSConnectState={} — {} — \
             synthetic keyboard/mouse input will not reach a real desktop",
            connect_state_name(state),
            connect_state_reason(state)
        );
    }
}

/// Query `session_id`'s [`WTS_CONNECTSTATE_CLASS`] via `WTSQuerySessionInformationW`, or `None`
/// if the query itself fails — the info class has existed since Vista so this is not expected,
/// but this diagnostic should degrade to silence rather than assume a state it never observed.
fn session_connect_state(session_id: u32) -> Option<WTS_CONNECTSTATE_CLASS> {
    let mut buffer = PWSTR::null();
    let mut bytes_returned: u32 = 0;
    // SAFETY: `WTS_CURRENT_SERVER_HANDLE` is a sentinel value meaning "the local RD Session
    // Host", not a real handle, so there is nothing here to validate or close; `session_id` was
    // just resolved by `ProcessIdToSessionId` for this very process; `buffer` and
    // `bytes_returned` are valid, uniquely-owned out-parameters for the duration of this call.
    // On success the call allocates the buffer `buffer` ends up pointing to, which must be
    // released with `WTSFreeMemory` — done by `WtsBuffer`'s `Drop`, below, once constructed.
    let ok = unsafe {
        WTSQuerySessionInformationW(
            WTS_CURRENT_SERVER_HANDLE,
            session_id,
            WTSConnectState,
            &mut buffer,
            &mut bytes_returned,
        )
    }
    .is_ok();
    if !ok || buffer.is_null() {
        return None;
    }
    let buffer = WtsBuffer(buffer);
    if (bytes_returned as usize) < std::mem::size_of::<i32>() {
        return None;
    }
    // SAFETY: the `WTSConnectState` info class returns a single `WTS_CONNECTSTATE_CLASS` (a
    // 4-byte value) in the buffer just allocated; `bytes_returned`, checked above, covers at
    // least that many bytes, and `buffer` (via `WtsBuffer`) keeps the allocation alive for this
    // read.
    let state = unsafe { *(buffer.0.as_ptr() as *const WTS_CONNECTSTATE_CLASS) };
    Some(state)
}

/// Frees the buffer `WTSQuerySessionInformationW` allocates, on every return path out of
/// [`session_connect_state`] once constructed — mirrors `appid::ProcessHandleGuard`.
struct WtsBuffer(PWSTR);

impl Drop for WtsBuffer {
    fn drop(&mut self) {
        // SAFETY: `self.0` was allocated by the successful `WTSQuerySessionInformationW` call in
        // `session_connect_state`, the only place this type is constructed, and is owned solely
        // by this guard.
        unsafe {
            WTSFreeMemory(self.0.as_ptr().cast());
        }
    }
}

/// A short name for `state`, for logging — the identifier the constant is actually named, not
/// its raw `i32`, which by itself says nothing to whoever is reading agent stderr. `==`
/// comparisons rather than pattern matching — see the comment in `log_session_context`.
fn connect_state_name(state: WTS_CONNECTSTATE_CLASS) -> &'static str {
    if state == WTSActive {
        "Active"
    } else if state == WTSConnected {
        "Connected"
    } else if state == WTSConnectQuery {
        "ConnectQuery"
    } else if state == WTSShadow {
        "Shadow"
    } else if state == WTSDisconnected {
        "Disconnected"
    } else if state == WTSIdle {
        "Idle"
    } else if state == WTSListen {
        "Listen"
    } else if state == WTSReset {
        "Reset"
    } else if state == WTSDown {
        "Down"
    } else if state == WTSInit {
        "Init"
    } else {
        "Unknown"
    }
}

/// Why `state` (never [`WTSActive`] — that case has its own message above) means input cannot
/// reach a real desktop, in terms drawn from `WTS_CONNECTSTATE_CLASS`'s own documentation rather
/// than guessed at.
fn connect_state_reason(state: WTS_CONNECTSTATE_CLASS) -> &'static str {
    if state == WTSDisconnected {
        "signed in but not connected (a disconnected RDP session, or a console session locked to \
         the lock screen) — nothing is attached to receive what gets drawn there"
    } else if state == WTSListen {
        "a listener session waiting for a connection — no user is logged on"
    } else if state == WTSIdle {
        "waiting for a client to connect — no user is logged on yet"
    } else if state == WTSInit || state == WTSConnectQuery || state == WTSReset {
        "still transitioning towards a connected state"
    } else if state == WTSDown {
        "down due to an error"
    } else if state == WTSShadow {
        "shadowing another session rather than hosting an interactive one of its own"
    } else if state == WTSConnected {
        "connected but not (yet) reported active — treated as not-yet-safe rather than assumed \
         to be"
    } else {
        "not in the Active state"
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
                    // The protocol's whole reason to exist is latency; Nagle's algorithm fights
                    // that on a link that mixes small control/input messages with bulk frame
                    // data — the client already disables it (`oxclient/src/main.rs`) for the
                    // same reason, but this is the side that sends the frames, which is the
                    // worse side to leave it on. A guest measurement found the classic Nagle /
                    // delayed-ACK stall (~40ms, both peers' default) dominating this session's
                    // p95/p99 end-to-end latency while leaving the median untouched — exactly
                    // the signature of a stream alternating small writes with an occasional
                    // large one, which every window's frame stream is. Logged and not fatal: a
                    // session with Nagle still enabled is degraded, not unusable, and dropping
                    // an otherwise-good connection over one socket option would be a worse
                    // outcome than the option itself.
                    if let Err(e) = tcp.set_nodelay(true) {
                        eprintln!("oxagent: {peer}: set_nodelay failed (continuing degraded): {e}");
                    }

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

    fn next_frame(
        &mut self,
        handle: isize,
        intent: CaptureIntent,
    ) -> Option<crate::serve::SourceFrame> {
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

        match capture.try_next_frame(intent) {
            Ok(Some(frame)) => Some(crate::serve::SourceFrame {
                width: frame.width.min(u32::from(u16::MAX)) as u16,
                height: frame.height.min(u32::from(u16::MAX)) as u16,
                data: frame.bgra,
                gpu_frame: frame.gpu_frame,
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
