//! Windows-only agent internals: window enumeration and per-window capture.
#![cfg(windows)]

mod appid;
pub mod capture;
pub mod enumerate;
mod input;

use std::process::ExitCode;

use oxsec::AgentIdentity;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};

use crate::config::AgentConfig;
use crate::serve::{run_session, SessionParams};
use capture::WindowCapture;
use enumerate::enumerate_windows;
use input::WinInputSink;

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

/// Agent entry point: set up TLS, bind the listener, and serve clients until shutdown.
///
/// Each connection is authenticated before the capture source is touched (see
/// [`crate::serve::run_session`]), and sessions are handled one at a time — a second client
/// would contend for the same windows, and multi-session support is out of scope for v0.
pub fn run_agent(config: &AgentConfig, print_pin: bool) -> ExitCode {
    set_dpi_awareness();

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

        let mut session_id: u64 = 1;
        loop {
            let (tcp, peer) = match listener.accept().await {
                Ok(pair) => pair,
                Err(e) => {
                    eprintln!("oxagent: accept: {e}");
                    continue;
                }
            };
            let tls = match acceptor.accept(tcp).await {
                Ok(s) => s,
                Err(e) => {
                    // A failed TLS handshake is routine (a port scan, a client with the wrong
                    // pin); log and keep serving.
                    eprintln!("oxagent: TLS handshake from {peer} failed: {e}");
                    continue;
                }
            };

            eprintln!("oxagent: session {session_id} from {peer}");
            let params = SessionParams {
                target_fps: config.target_fps,
                max_frames_in_flight: config.max_frames_in_flight,
            };
            let mut source = WinWindowSource::new();
            let mut sink = WinInputSink::new();
            match run_session(tls, &mut source, &mut sink, params, session_id, &token).await {
                Ok(negotiated) => eprintln!(
                    "oxagent: session {session_id} with '{}' ended",
                    negotiated.client_name
                ),
                Err(e) => eprintln!("oxagent: session {session_id}: {e}"),
            }
            session_id += 1;
        }
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
            // A minimized window has no meaningful geometry and nothing to capture.
            .filter(|w| !w.minimized)
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
                    // Always `false` here: the filter above already dropped minimized windows,
                    // so no `WindowInfo` with `minimized: true` ever reaches this point. Wired
                    // through anyway so `sync_windows` computes `flags` from the real field
                    // rather than a hardcoded constant, and so it takes over correctly if that
                    // filter is ever relaxed.
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
