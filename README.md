# oxrdp

**English** | [한국어](docs/README.ko.md)

[![license](https://img.shields.io/github/license/kernalix7/oxrdp?style=flat-square&color=blue)](LICENSE)
[![status](https://img.shields.io/badge/status-pre--alpha-orange?style=flat-square)](#status)
[![language](https://img.shields.io/badge/rust-stable-DEA584?style=flat-square&logo=rust&logoColor=white)](https://www.rust-lang.org/)

**A memory-safe RDP client written in Rust, built for seamless integration of Windows applications into the Linux desktop.**

oxrdp connects to a Windows RDP server and renders remote apps as native Linux windows via RAIL / RemoteApp — real titles, real `WM_CLASS`, pinnable and alt-tabbable — prioritizing Rust's safety guarantees, minimal resource usage, and high performance.

oxrdp is the RDP engine behind [winpodx](../00G_winpodx), split out as a standalone project. It exists to replace winpodx's current dependency on FreeRDP 3.x and to fix the limitations that dependency imposes.

---

## Why oxrdp exists

winpodx today shells out to `xfreerdp3` (FreeRDP 3.x) to surface Windows apps as native Linux windows through RemoteApp / RAIL. That works, but the FreeRDP dependency is the source of recurring pain:

- **RAIL window-mapping correctness** — z-order, missing windows, popups / drop-downs / tooltips, taskbar and `WM_CLASS` mapping bugs that vary by FreeRDP point-release (e.g. broken RemoteApp windows below 3.6.0).
- **Performance & input latency** — GFX H.264 / AVC444 negotiation, bandwidth, frame rate, and round-trip input lag.
- **Memory safety & stability** — a large C codebase in the critical path; crashes and undefined behavior land on the user.
- **Feature gaps & friction** — clipboard, audio in/out, multi-monitor strategy, HiDPI scaling, and device redirection each carry FreeRDP-version-specific quirks.

oxrdp's thesis: **own the protocol stack in safe Rust**, designed from day one around RAIL and native Linux window integration, so these become engineering decisions we control instead of upstream quirks we work around.

## Project decisions (locked)

| Decision | Choice | Rationale |
| --- | --- | --- |
| **Protocol stack** | Implemented from scratch in Rust | Full control, zero FreeRDP dependency, true memory-safe core. |
| **Low-level building blocks** | Reuse vetted crates | TLS via `rustls`+`ring`; NLA/CredSSP via `sspi-rs` (deferred); video decode via `openh264`/`dav1d` bindings; async IO via `tokio`. "From scratch" = the RDP protocol, RAIL, and rendering — not crypto/codec primitives. |
| **Core architecture** | sans-io state machine | Pure, IO-free protocol core (à la IronRDP) with pluggable IO / render / input shells. Buys testability, fuzzing, and X11+Wayland reuse. |
| **Display backend** | X11 + Wayland behind one abstraction | A `DisplayBackend` trait; each remote RAIL window maps to one native toplevel. X11 backend first (matches today's deployment), Wayland alongside. |
| **Rendering & decode** | `wgpu` GPU from the start; VA-API HW decode | Compositing/scaling/present via `wgpu`. H.264 GFX decode is VA-API hardware-first with an `openh264` software fallback; VA-API frames import to `wgpu` via DMA-BUF (zero-copy). |
| **Keymap** | Hybrid (host XKB + table fallback) | `xkbcommon` reads the host layout (correct Hangul/CJK), falling back to a shipped table when none resolves. |
| **Protocol surface** | Staged | v0 forces the modern, narrow surface (we control the guest); broaden toward general RDP-server compatibility later. |
| **winpodx integration** | Rust library + thin binary; v0 = `oxrdp-cli` subprocess + IPC | winpodx (Python) spawns `oxrdp-cli` and drives it over a socket/JSON control channel. In-process C-ABI `cdylib` FFI is a post-v0 option. |
| **v0 success criterion** | **Drop-in equivalence with winpodx's FreeRDP path** | v0 is "done" when winpodx runs its RAIL multi-app workflow on oxrdp instead of `xfreerdp3`, at parity. |

## Scope

**In scope (eventually):** the full set of FreeRDP capabilities winpodx relies on — see the parity matrix in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md). RAIL/RemoteApp is the headline feature, not an afterthought.

**v0 target surface (drop-in parity):**
- Connect + logon (`/v /u /d /p`) over **TLS security** (`/sec:tls`) with trust-on-first-use certs (`/cert:tofu|ignore`). **NLA/CredSSP is deferred** — winpodx deliberately uses `/sec:tls` to avoid the NLA path, so v0 does not need it.
- RAIL / RemoteApp launch (`/app:program,name,cmd`), `WM_CLASS` mapping, keyboard grab.
- Graphics: GFX pipeline (H.264 AVC420/AVC444) with RemoteFX fallback; bitmap as last resort.
- Channels: clipboard (cliprdr), audio out (rdpsnd), filesystem redirection (`\\tsclient`, rdpdr).
- Display: multi-monitor (RAIL-primary + span), HiDPI scaling, dynamic resolution (desktop mode).

**Deferred / staged:** NLA/CredSSP & Kerberos, microphone (audin), printer, USB / smartcard / serial / parallel redirection, broad compatibility with arbitrary (non-winpodx-controlled) RDP servers.

## Status

Pre-alpha — specification and scaffolding. No working client yet. See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the workspace layout and milestone roadmap.

## License

TBD (intended permissive — Apache-2.0 / MIT, matching the Rust ecosystem and winpodx's distribution model).
