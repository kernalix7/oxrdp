# Features

**English** | [한국어](FEATURES.ko.md)

> [!IMPORTANT]
> **Superseded (2026-07-02).** This document describes the original design, in which oxrdp was
> an RDP *client*. The project has since pivoted to **replacing RDP** with a custom low-latency
> protocol (a Windows guest agent streaming individual app windows to a Linux client). Current
> state: [`docs/HANDOFF.md`](HANDOFF.md) · protocol: [`docs/design/OXPROTO.md`](design/OXPROTO.md)
> · gaps: [`docs/design/AUDIT-2026-07.md`](design/AUDIT-2026-07.md). Kept for the parts that
> still apply (the client shells, the codec base) and for history.


> **Status: pre-alpha.** Nothing below is shippable yet. This page tracks the intended
> capability set and its milestone. "v0" = required for drop-in equivalence with winpodx's
> FreeRDP path; "Staged" = deferred per the staged protocol-surface decision. See
> [ARCHITECTURE.md](ARCHITECTURE.md) for the engineering detail and
> [the roadmap](ARCHITECTURE.md#5-milestone-roadmap) for sequencing.

## Headline

oxrdp renders Windows apps as **native Linux windows** via RAIL / RemoteApp — real titles,
real `WM_CLASS`, pinnable and alt-tabbable — over a memory-safe, from-scratch Rust RDP
stack. RAIL is the headline feature, not an afterthought.

## Capability matrix

| Capability | Detail | Status |
|---|---|---|
| Connect + logon | TCP, X.224/MCS, capability exchange | v0 |
| TLS security | `rustls`, trust-on-first-use / pinned / system-CA cert policy | v0 |
| NLA / CredSSP | NTLM / Kerberos via `sspi-rs` | Staged (winpodx avoids it via TLS) |
| RAIL / RemoteApp | Remote-window list, z-order, popups, icons, move/resize | v0 |
| Native window mapping | One native toplevel per remote window; `WM_CLASS`, title, icon | v0 |
| Keyboard grab | `+grab-keyboard` equivalent | v0 |
| GFX pipeline | H.264 AVC420 / AVC444 | v0 |
| RemoteFX | Negotiated fallback | v0 |
| Bitmap codecs | Interleaved / planar, last-resort | v0 |
| Hardware decode | VA-API, DMA-BUF zero-copy into `wgpu` | v0 (software `openh264` fallback) |
| Clipboard | cliprdr, both directions | v0 |
| Audio out | rdpsnd | v0 |
| Drive redirection | `\\tsclient`, rdpdr | v0 |
| Multi-monitor | RAIL-primary + span | v0 |
| HiDPI scaling | per-monitor scale factors | v0 |
| Dynamic resolution | full-desktop resize | v0 |
| Microphone in | audin | Staged |
| Printer redirection | rdpdr printer | Staged |
| USB / smartcard / serial / parallel | device redirection | Staged |
| General RDP-server compatibility | arbitrary (non-winpodx) servers | Staged |

## Display backends

| Backend | Library | Status |
|---|---|---|
| X11 | `x11rb` | v0 (first) |
| Wayland | `smithay-client-toolkit` / `wayland-client` | v0 (parity at M4) |

Both sit behind one `DisplayBackend` trait; the protocol core is identical for both.
