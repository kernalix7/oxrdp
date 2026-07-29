//! `oxagent` — the oxrdp Windows guest agent.
//!
//! Captures individual application windows via Windows.Graphics.Capture and streams them to
//! the Linux client over the oxproto protocol, behind mandatory TLS with a pinned certificate
//! and a shared token. See `docs/design/agent-runtime.md` for the session and deployment
//! model, and `docs/design/OXPROTO.md` for the wire protocol.
//!
//! The handshake, pacing, window bookkeeping and session driver are platform-independent and
//! unit-tested on the build host; only capture and (later) input injection are Windows-only.
#![allow(unsafe_code)] // windows-rs COM/WinRT calls require unsafe

pub mod config;
pub mod encode;
pub mod h264;
pub mod handshake;
pub mod input;
pub mod nv12;
pub mod pacing;
pub mod registry;
pub mod serve;

#[cfg(windows)]
mod win;

use std::path::PathBuf;
use std::process::ExitCode;

use config::AgentConfig;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let prog = args.first().map(String::as_str).unwrap_or("oxagent");

    if args.iter().any(|a| a == "--help" || a == "-h") {
        eprintln!("usage: {prog} [--config <path>] [--print-pin]");
        eprintln!("  --config     configuration file (default: oxagent.conf)");
        eprintln!("  --print-pin  print the TLS certificate pin the client must use, then exit");
        return ExitCode::SUCCESS;
    }

    // `--print-pin` exists so whatever provisions the guest can read the value the client has
    // to pin, without the agent ever printing its private key.
    let print_pin = args.iter().any(|a| a == "--print-pin");
    let config_path = args
        .iter()
        .position(|a| a == "--config")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("oxagent.conf"));

    let config = match AgentConfig::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("oxagent: {}: {e}", config_path.display());
            return ExitCode::from(2);
        }
    };

    #[cfg(windows)]
    {
        win::run_agent(&config, print_pin)
    }
    #[cfg(not(windows))]
    {
        // The agent is only useful on the guest, but keeping `main` buildable on the host is
        // what lets the driver's tests run in CI.
        let _ = (config, print_pin);
        eprintln!("oxagent runs on the Windows guest; build with --target x86_64-pc-windows-gnu");
        ExitCode::from(2)
    }
}
