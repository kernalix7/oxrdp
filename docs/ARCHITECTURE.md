# oxrdp — Architecture

This document records the engineering shape of oxrdp: the sans-io core, the crate
workspace, the display-backend abstraction, the FreeRDP parity matrix that defines
the v0 "drop-in equivalence" bar, and the milestone roadmap.

It is the source of truth for project structure. The high-level rationale lives in
[`../README.md`](../README.md).

---

## 1. Design principle: sans-io core, impure shells

The protocol logic never touches a socket, a clock, or a window. It is a set of
**pure state machines** that consume bytes/events and emit *outputs* (bytes to send,
surface updates, window lifecycle events). All IO, timing, rendering, and input
capture live in thin "shell" crates that drive the core.

```
            ┌──────────────────────── impure shells ────────────────────────┐
  network → │  oxrdp-io (tokio)  ──bytes──►  ┌─────────────────────────────┐  │
            │                                │        sans-io core         │  │
  display ← │  oxrdp-display     ◄─surfaces──┤  pdu · core · graphics ·    │  │
  (X11/Wl)  │  (X11 + Wayland)   ──events──► │  rail · channels            │  │ ← pure, no IO
            │                                └─────────────────────────────┘  │
  input   → │  oxrdp-input       ──input───►                                   │
            └────────────────────────────────────────────────────────────────┘
```

Why this matters here specifically:
- **X11 + Wayland with one core.** The backend abstraction is the *only* place the
  windowing system appears; the protocol code is identical for both.
- **Testability & fuzzing.** The core is a `feed(bytes) -> Vec<Output>` function —
  deterministic, replayable from captured RDP traces, fuzzable without a server.
- **RAIL correctness as data.** Remote window state (list, z-order, icons, geometry,
  parent/owner, popup vs toplevel) is modeled explicitly in `oxrdp-rail` and verified
  in isolation, instead of being implicit in render side-effects — directly targeting
  the FreeRDP RAIL bugs that motivated the project.

## 2. Crate workspace

A Cargo workspace. Pure crates have no `tokio` / windowing deps.

| Crate | Purity | Responsibility |
| --- | --- | --- |
| `oxrdp-pdu` | pure | Wire types: encode/decode of every PDU we speak. The protocol vocabulary. |
| `oxrdp-core` | pure | Connection state machine: X.224, MCS, capability exchange, channel join, the connection sequence. |
| `oxrdp-graphics` | pure | GFX/RFX/bitmap **protocol** + surface/region model. Emits codec-tagged decode commands (`decode H.264/RFX/bitmap payload → surface S @ region R`); it does **not** run hardware decoders — actual pixel decode lives in the render shell. |
| `oxrdp-render` | shell | H.264 decode (**VA-API** HW, `openh264` SW fallback), RFX/bitmap CPU decode, and `wgpu` compositing/present. VA-API frames import to `wgpu` via DMA-BUF (zero-copy). |
| `oxrdp-channels` | pure | Static/dynamic virtual channels: cliprdr (clipboard), rdpsnd (audio out), rdpdr (drive/printer), audin (mic). |
| `oxrdp-rail` | pure | RAIL / RemoteApp: remote window list, ordering, icons, move/resize/minimize, popups, language-bar, sysmenu. The heart of "seamless." |
| `oxrdp-crypto` | thin | Security glue: `rustls` TLS, and (deferred) `sspi-rs` NLA/CredSSP. Sits between `oxrdp-io` and the core. |
| `oxrdp-io` | shell | `tokio` transport: TCP, TLS stream, framing, the async driver that pumps the sans-io core and flushes its outputs. |
| `oxrdp-display` | shell | `DisplayBackend` trait + `x11` and `wayland` backends. One native toplevel per remote RAIL window. |
| `oxrdp-input` | shell | Keyboard/mouse/touch capture → input PDUs; keyboard-grab semantics; scancode/keymap translation. |
| `oxrdp` | lib | High-level `Session` API that wires core + shells. **This is what winpodx links.** |
| `oxrdp-cli` | bin | Thin binary: flag parsing, config, wires `oxrdp` to a chosen backend. |

Dependency direction: shells → `oxrdp` (lib) → pure core crates. Pure crates never
depend on shells.

## 3. Display backend abstraction

Split of responsibility: `oxrdp-display` owns **native window lifecycle, metadata, and
input**, and hands each window a `raw-window-handle` that `oxrdp-render` binds a `wgpu`
surface to. `oxrdp-render` owns the `wgpu` device, codec decode, and drawing. So
`present` below means "a decoded region is ready" — the actual GPU draw happens in the
render shell against the window's `wgpu` surface.

```rust
/// One implementation per windowing system (X11, Wayland).
trait DisplayBackend {
    /// A remote RAIL window appeared — create a native toplevel.
    fn create_window(&mut self, id: RemoteWindowId, attrs: &WindowAttrs) -> Result<()>;
    /// Blit a decoded surface region into a window.
    fn present(&mut self, id: RemoteWindowId, region: &SurfaceRegion) -> Result<()>;
    /// Title / WM_CLASS / icon / min-max / parent — the metadata that makes it feel native.
    fn set_metadata(&mut self, id: RemoteWindowId, meta: &WindowMeta) -> Result<()>;
    fn destroy_window(&mut self, id: RemoteWindowId) -> Result<()>;
    /// Pump native events (resize, move, focus, close, input) back toward the core.
    fn poll_events(&mut self) -> Vec<BackendEvent>;
}
```

- **X11 backend** (`x11rb`): override-redirect / normal toplevels per remote window;
  `WM_CLASS`, `_NET_WM_*` hints, `_NET_WM_ICON`; this is where the winpodx
  `MonitorDefArray`/`/multimon` layout concerns live.
- **Wayland backend** (`smithay-client-toolkit`): `xdg_toplevel` per remote window.
  Note the model constraint — Wayland clients can't set absolute window positions,
  so RAIL geometry semantics differ from X11 and are handled here, not in the core.

## 4. FreeRDP → oxrdp parity matrix (the v0 bar)

Derived from the exact `xfreerdp3` flags winpodx emits in `winpodx/core/rdp.py`.
"v0" = required for drop-in equivalence; "Staged" = deferred per the staged
protocol-surface decision.

| FreeRDP flag(s) (winpodx) | Capability | oxrdp component | v0? |
| --- | --- | --- | --- |
| `/v /u /d /p` | Connect + logon | `oxrdp-core` | **v0** |
| `/sec:tls`, `/cert:ignore\|tofu` | TLS security + TOFU cert | `oxrdp-crypto` | **v0** |
| *(NLA / CredSSP)* | Network Level Auth | `oxrdp-crypto` (`sspi-rs`) | Staged — winpodx avoids it via `/sec:tls` |
| `/app:program,name,cmd`, `/app-name`, `/app-cmd` | RAIL / RemoteApp launch | `oxrdp-rail` | **v0** |
| `/wm-class` | `WM_CLASS` on native window | `oxrdp-display` | **v0** |
| `+grab-keyboard` | Keyboard grab | `oxrdp-input` | **v0** |
| `/gfx` (`h264`, `progressive`, `thin-client`, `small-cache`, `RFX`) | GFX graphics pipeline | `oxrdp-graphics` | **v0** (H.264 AVC420/444) |
| `/rfx` | RemoteFX fallback | `oxrdp-graphics` | **v0** |
| `/compression`, `/network`, `/codec`, `/bpp` | Perf / codec tuning | `oxrdp-core` + `oxrdp-graphics` | **v0** |
| clipboard (cliprdr, FreeRDP default) | Clipboard sync (both ways) | `oxrdp-channels` | **v0** |
| `/sound:sys:alsa` | Audio out (rdpsnd) | `oxrdp-channels` | **v0** |
| `/drive:home`, `/drive:media,<base>` | Filesystem redir (`\\tsclient`) | `oxrdp-channels` (rdpdr) | **v0** |
| `/multimon`, `/span`, `/smart-sizing`, `/size`, `/monitors` | Multi-monitor layout | `oxrdp-display` + `oxrdp-core` | **v0** (RAIL-primary + span) |
| `/scale`, `/scale-desktop`, `/scale-device` | HiDPI scaling | `oxrdp-display` | **v0** |
| `/dynamic-resolution` | Dynamic resize (desktop mode) | `oxrdp-channels` (dynvc) | **v0** (full-desktop mode) |
| `/window-position` | Initial window position | `oxrdp-display` | Nice-to-have |
| `/microphone` | Audio in (audin) | `oxrdp-channels` | Staged |
| `/printer` | Printer redir | `oxrdp-channels` (rdpdr) | Staged |
| `/usb:auto` | USB redirection | `oxrdp-channels` | Staged |
| `/smartcard`, `/serial`, `/parallel` | Device redir | `oxrdp-channels` | Staged |
| `/gdi:sw\|hw` | GDI repaint mode | — | N/A (own renderer) |

### winpodx-specific quirks the parity work must honor

- **Combined `/app:program:X,name:Y,cmd:Z` syntax** — FreeRDP 3's RAIL parse splits on
  commas; oxrdp's `Session` API takes these as structured fields, and the winpodx
  adapter maps to them. No shell-string parsing needed on our side.
- **`/span` vs `/multimon` for RAIL** — FreeRDP RAIL can't span a non-contiguous
  monitor layout; winpodx retries without `/span|/multimon` when the layout doesn't
  tile. oxrdp models the host monitor layout explicitly and pins RAIL windows to the
  primary monitor, sidestepping the `MonitorDefArray` failure mode.
- **GFX under XWayland** — winpodx has a `/gfx:RFX` fallback (force RemoteFX, skip H.264)
  for an XWayland GFX-surface mapping bug. oxrdp's own renderer removes the XWayland
  surface dependency, but the RFX path is kept as a negotiated fallback regardless.

## 5. Milestone roadmap

Sequenced so each milestone is independently demonstrable. v0's success bar is
drop-in equivalence, so it is intentionally large; the sub-milestones make it tractable.

- **M0 — Scaffold & handshake.** Workspace, sans-io test harness, replay of captured
  RDP traces. `oxrdp-core` reaches the connection-sequence / capability-exchange stage
  against the dockur guest over `/sec:tls`. No pixels yet.
- **M1 — First pixels (desktop).** GFX H.264 + RFX decode; full-desktop session renders
  and takes keyboard/mouse input through one backend (X11 first). Proves the IO ↔ core
  ↔ display ↔ input loop end-to-end.
- **M2 — First RAIL window.** `oxrdp-rail` maps a single RemoteApp window to a native
  toplevel with correct `WM_CLASS`, title, icon, and input. The vertical slice that is
  winpodx's whole reason to exist.
- **M3 — RAIL multi-app & channels.** Multiple concurrent RemoteApp windows with correct
  z-order/popups; clipboard, audio-out, and `\\tsclient` drive redirection.
- **M4 — Display parity.** Multi-monitor (RAIL-primary + span), HiDPI scaling, dynamic
  resolution. Wayland backend reaches parity with X11.
- **M5 — Drop-in equivalence (v0).** winpodx runs its RAIL multi-app workflow on oxrdp
  via the `oxrdp` library, at parity with the FreeRDP path. **v0 ships.**
- **Post-v0 (staged surface).** NLA/CredSSP (`sspi-rs`), microphone, printer, USB and
  other device redirection, and broadening toward arbitrary RDP-server compatibility.

## 6. winpodx integration shape

Integration is **library + thin binary**, but the v0 *bar* is drop-in equivalence.
Reconciliation: `oxrdp` exposes a structured `Session` API; winpodx's `core/rdp.py`
gains a small adapter that builds an `oxrdp` session config (the same capabilities it
encodes today as `xfreerdp3` flags) and either links the library or invokes
`oxrdp-cli`. winpodx does **not** need to keep emitting FreeRDP-style flag strings —
the capabilities, not the CLI syntax, are what must reach parity.

## 7. Resolved decisions (round 3)

- **Renderer: GPU from the start (`wgpu`).** Window compositing, scaling, and present
  go through `wgpu` rather than per-window CPU blits. Raises the performance ceiling and
  pairs with VA-API decode below.
- **H.264 GFX decode: VA-API hardware first, `openh264` software fallback.** VA-API for
  lower CPU/latency and 4K headroom; `openh264` keeps it working where VA-API is
  unavailable. Decoded frames stay on the GPU — VA-API output is imported into `wgpu` via
  **DMA-BUF (zero-copy)** and presented without a CPU round-trip; the software path
  uploads to a `wgpu` texture.
- **Keymap: hybrid — host XKB-derived with built-in table fallback.** `xkbcommon` reads
  the user's actual host layout (correct Hangul/CJK/non-US), falling back to a shipped
  table when no host keymap is resolvable.
- **Library boundary: `oxrdp-cli` subprocess + IPC for v0.** Matches the round-1
  thin-binary choice — winpodx (Python) spawns `oxrdp-cli` and drives it over a
  socket/JSON control channel. A C-ABI `cdylib` for in-process FFI is a post-v0 option,
  not a v0 requirement.
