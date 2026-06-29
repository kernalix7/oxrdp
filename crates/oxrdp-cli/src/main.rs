//! `oxrdp-cli` — thin binary that launches an [`oxrdp`] session.
//!
//! Pre-alpha: not yet implemented (skeleton). For v0, winpodx spawns this binary and
//! drives it over a socket/JSON control channel; see
//! [docs/ARCHITECTURE.md](https://github.com/kernalix7/oxrdp/blob/main/docs/ARCHITECTURE.md).
#![forbid(unsafe_code)]

fn main() {
    eprintln!(
        "oxrdp {} — pre-alpha; the RDP client is not yet implemented.",
        env!("CARGO_PKG_VERSION")
    );
    eprintln!("See https://github.com/kernalix7/oxrdp for the roadmap.");
}
