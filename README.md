# oxrdp

**English** | [한국어](docs/README.ko.md)

[![license](https://img.shields.io/github/license/kernalix7/oxrdp?style=flat-square&color=blue)](LICENSE)
[![status](https://img.shields.io/badge/status-pre--alpha-orange?style=flat-square)](#status)
[![language](https://img.shields.io/badge/rust-stable-DEA584?style=flat-square&logo=rust&logoColor=white)](https://www.rust-lang.org/)

**A memory-safe, low-latency remote-application system in Rust: individual Windows applications
appear as native Linux windows.**

A **Windows guest agent** captures each application window and streams it over a purpose-built
protocol to a **Linux client**, which maps every remote window to a real native window — correct
title, `WM_CLASS`, icon, pinnable, alt-tabbable. Not a desktop in a box: one app, one window.

oxrdp is the engine behind [winpodx](https://github.com/kernalix7/winpodx), split out as a
standalone project.

---

## Why it exists

winpodx surfaces Windows apps as Linux windows through FreeRDP's RemoteApp. That works, but the
RDP path imposes limits that cannot be fixed from the client side:

- **Latency is structural.** RDP is bandwidth-optimized and TCP-first: head-of-line blocking on
  loss, buffering that trades latency for efficiency, and general-purpose overhead a
  purpose-built app-streaming protocol does not have to pay.
- **Seamless-window correctness is upstream's.** RAIL z-order, popups, taskbar and `WM_CLASS`
  mapping bugs vary by FreeRDP point-release.
- **A large C codebase sits in the critical path**, so crashes and undefined behavior land on
  the user.

So oxrdp replaces the protocol itself — the same move RustDesk and Moonlight make — rather than
writing a better client for someone else's.

> **History.** oxrdp began as a from-scratch Rust *RDP client*; that stack was built and
> validated end to end against a real Windows host (through MCS channel join) before the project
> pivoted on 2026-07-02. It is shelved but kept in git, and its client shells and
> bounds-checked codec are reused. Pre-pivot documents carry a "Superseded" banner.

## How it works

```
[Linux] oxclient  ──oxproto (TCP+TLS now, QUIC planned)──▶  [Windows guest] oxagent
  · decode (VA-API / wgpu)                                    · enumerate app windows (Win32)
  · one native X11/Wayland window per remote window           · capture per window (WGC)
  · capture input, send it back                               · encode (Media Foundation / SW)
                                                              · inject input
```

## Design decisions

| Decision | Choice | Rationale |
| --- | --- | --- |
| **What we replace** | RDP itself — custom protocol + guest agent | The latency limits are in the protocol, not the client. |
| **Protocol** | [`oxproto`](docs/design/OXPROTO.md): chunked, channelled, sans-io | Fragmentation + per-channel priority so a keyframe cannot delay input; frame acks so latency cannot grow unbounded; per-type size limits because the agent parses untrusted input. |
| **Guest agent** | Rust + `windows-rs`, Windows.Graphics.Capture | Per-window GPU capture; memory-safe outside the audited FFI. Cross-compiled from Linux. |
| **Encoding** | Runtime select: Media Foundation HW → SW fallback | Hardware where the guest has it; `RAW_BGRA` for bring-up only. |
| **Transport** | QUIC preferred, TCP fallback | QUIC's independent streams remove the last head-of-line blocking on lossy links. |
| **Client rendering** | `wgpu` GPU, VA-API decode | The DMA-BUF import path is **unvalidated** and needs a `wgpu_hal` spike. |
| **Display backends** | X11 + Wayland behind one trait | One native toplevel per remote window. |
| **Security** | Mandatory TLS + pinned agent cert + auth token | The agent shares screen content and injects input; it must never serve an unauthenticated peer. See [SECURITY.md](SECURITY.md). |

## Status

**Pre-alpha.** The protocol, its framing, and the transport are implemented and tested; the
agent captures windows; the client performs the handshake and event loop. Nothing renders yet.

| Component | State |
| --- | --- |
| `oxproto` — protocol messages, framing, limits | implemented, tested, fuzzed |
| `oxtransport` — async chunk IO | implemented, tested |
| `oxclient` — handshake + event loop | implemented, tested |
| `oxagent` — window enumeration, WGC capture | implemented (cross-compiles); no listener yet |
| display / render / input | not started |

Current state and next steps: [`docs/HANDOFF.md`](docs/HANDOFF.md).
Known gaps, adversarially verified: [`docs/design/AUDIT-2026-07.md`](docs/design/AUDIT-2026-07.md).

## Build

```bash
cargo test --workspace                                   # Linux side
cargo build -p oxagent --target x86_64-pc-windows-gnu    # the Windows agent, cross-compiled
```

The toolchain and the Windows target are pinned in `rust-toolchain.toml`; the agent's Windows
dependencies are `cfg(windows)`-gated, so the workspace builds on Linux with the agent as a stub.

## Documentation

| | |
| --- | --- |
| Current state, roadmap, how to continue | [`docs/HANDOFF.md`](docs/HANDOFF.md) |
| Protocol wire specification | [`docs/design/OXPROTO.md`](docs/design/OXPROTO.md) |
| Agent runtime model (session, deployment) | [`docs/design/agent-runtime.md`](docs/design/agent-runtime.md) |
| Gap audit | [`docs/design/AUDIT-2026-07.md`](docs/design/AUDIT-2026-07.md) |
| Security posture | [`SECURITY.md`](SECURITY.md) |

## Name

`oxrdp` is now a misnomer — the project no longer speaks RDP. The crates keep their `oxrdp-*`
names for the shelved client; the new components are `oxproto` / `oxtransport` / `oxagent` /
`oxclient`. A rename is deliberately deferred until the first end-to-end milestone.

## License

MIT — see [LICENSE](LICENSE).
