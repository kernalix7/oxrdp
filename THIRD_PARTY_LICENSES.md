# Third-Party Licenses

oxrdp is MIT-licensed (see [LICENSE](LICENSE)). oxrdp implements the RDP protocol, RAIL,
and rendering from scratch, but reuses vetted Rust crates for low-level building blocks
(cryptography, authentication, codec decode, async IO, windowing). This document lists the
planned dependency set and upstream licenses.

> **Status: pre-alpha.** The Cargo workspace is not yet populated, so the exact crate
> graph and versions are not final. Once `Cargo.toml` exists, a precise, auto-generated
> `THIRD_PARTY_LICENSES.txt` will be produced from the crate graph with
> [`cargo-about`](https://github.com/EmbarkStudios/cargo-about) / `cargo-deny` in CI, and
> this file will link to it. The table below is the intended set, per the locked
> architecture decisions (see [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)).

## Crates are not vendored

oxrdp does not vendor or redistribute its dependencies in the source tree; Cargo resolves
and fetches them from crates.io at build time. The licenses below govern the resulting
binary, which links them.

## Planned dependency set

| Crate | Purpose | License (typical) |
|-------|---------|-------------------|
| [`rustls`](https://github.com/rustls/rustls) | TLS for the RDP security layer | Apache-2.0 OR MIT OR ISC |
| [`ring`](https://github.com/briansmith/ring) | Cryptographic primitives behind rustls | ISC-style + OpenSSL/BoringSSL (see crate) |
| [`sspi`](https://github.com/Devolutions/sspi-rs) (sspi-rs) | NLA / CredSSP / NTLM / Kerberos (**deferred**, post-v0) | MIT OR Apache-2.0 |
| [`tokio`](https://github.com/tokio-rs/tokio) | Async runtime for the IO shell | MIT |
| [`wgpu`](https://github.com/gfx-rs/wgpu) | GPU compositing / present | MIT OR Apache-2.0 |
| [`openh264`](https://github.com/ralfbiedert/openh264-rust) (bindings) | H.264 software decode fallback | BSD-2-Clause (bindings); see Cisco note below |
| [`dav1d`](https://code.videolan.org/videolan/dav1d) (bindings) | AV1 decode (if/when negotiated) | BSD-2-Clause |
| VA-API (`libva` via FFI) | Hardware H.264 decode (primary) | MIT |
| [`x11rb`](https://github.com/psychon/x11rb) | X11 display backend | MIT OR Apache-2.0 |
| [`smithay-client-toolkit`](https://github.com/Smithay/client-toolkit) / `wayland-client` | Wayland display backend | MIT |
| [`xkbcommon`](https://github.com/rust-x-bindings/xkbcommon-rs) (bindings to libxkbcommon) | Keymap / scancode translation | MIT (bindings); libxkbcommon is MIT |

## Note on H.264 / openh264 (Cisco)

OpenH264 is BSD-2-Clause as source, but Cisco's prebuilt binary distribution carries a
separate royalty arrangement (Cisco pays the MPEG-LA license fees for *its* binaries).
Projects that ship the Cisco-provided binary inherit that arrangement; projects that build
openh264 from source, or that decode via the system VA-API stack, do not. oxrdp's primary
decode path is **system VA-API** (hardware), with openh264 as a portable software fallback.
The packaging decision for how the software decoder is obtained (system package vs.
build-from-source vs. Cisco binary) will be documented here when releases begin, so the
H.264 patent/royalty posture is explicit for redistributors.

## Reference projects (inspiration only, no code redistributed)

- **FreeRDP** (https://github.com/FreeRDP/FreeRDP, Apache-2.0) — the C client oxrdp aims to
  replace in winpodx. Used as a protocol-behavior reference; no source is copied.
- **IronRDP** (https://github.com/Devolutions/IronRDP, MIT/Apache-2.0) — a Rust RDP stack
  whose sans-io design informs oxrdp's architecture. oxrdp implements its own stack; no
  source is copied.

If you find any attribution gap, please open an issue.
