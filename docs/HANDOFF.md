# oxrdp — Continuation handoff

Snapshot for continuing this project in another tool (e.g. opencode). Everything needed to
resume is here; the full history is in git (`github.com/kernalix7/oxrdp`, branch `main`).

Last state: **P1d done — the agent serves authenticated TLS sessions and streams windows; the
client connects end to end and models window state. Display architecture decided; P2b (first
pixels) next.** See [Roadmap & next steps](#8-roadmap--next-steps).

## 1. TL;DR

- **The project pivoted** (2026-07-02) from "a better RDP *client*" to **replacing RDP itself**
  (RustDesk / Moonlight style): a **Windows guest agent** captures individual app windows and
  streams them to a **Linux client** over a **custom, low-latency protocol**. Rationale: RDP has
  structural latency limits (TCP head-of-line blocking by default, bandwidth-optimized buffering,
  general-purpose overhead) a purpose-built protocol can beat.
- The earlier **RDP-client** stack was built and **validated end-to-end against a real Windows
  guest** (through MCS channel join) before the pivot. It is **shelved** but kept in git; its
  client shells + codec base are reused.
- **The agent now serves.** TLS with a pinned self-signed identity (`oxsec`), a
  token-authenticated handshake, and per-window frame streaming with flow control (P1d). **The
  client connects end to end**: `oxclient` performs the handshake and turns the event stream
  into an ordered `WindowModel` a display backend can execute against. Neither side has been run
  against a real Windows guest yet, and nothing presents a pixel — that is P2b.
- **The client display/render architecture is decided**:
  [`design/client-display.md`](design/client-display.md) — winit + an x11rb sidecar owns
  windowing (permanent), a CPU `softbuffer` presenter blits first pixels (P2b), a `wgpu`
  presenter in a new `oxrender` crate arrives only at P5 (H.264). Supersedes the `DisplayBackend`
  sketch in `ARCHITECTURE.md` §3.
- Build is green: `cargo test --workspace` = **179 tests**, `cargo clippy --workspace -- -D warnings`
  clean on Linux *and* on the Windows target, and the agent **cross-compiles from Linux** to
  `x86_64-pc-windows-gnu` (also enforced in CI).
- A multi-agent **gap audit** (56 verified findings) drove a protocol redesign and a round of
  hardening. Read [`design/AUDIT-2026-07.md`](design/AUDIT-2026-07.md) before planning work —
  it is the most useful document in the repo for deciding what to do next.

## 2. Target architecture (post-pivot)

```
[Linux] oxclient  ──custom protocol (oxproto; TCP now, QUIC planned)──▶  [Windows guest] oxagent
  · decode (VA-API / wgpu)                                                 · enumerate app windows (Win32)
  · map each remote window → native Linux window (X11/Wayland)             · capture per window (Windows.Graphics.Capture)
  · capture Linux input → send                                            · encode (Media Foundation HW / SW)
                                                                          · inject input, clipboard, audio (later)
```

The **hard part** (be honest): per-app *seamless* windows = re-implementing RemoteApp's server
side in the agent (WGC per-window capture + window events + z-order). RDP/RemoteApp gave this for
free; the custom protocol must build it. This is why it's a bigger project than the RDP client —
justified by latency + full control.

## 3. Locked decisions (agent architecture)

- Guest agent stack: **Rust + `windows-rs`** (0.58).
- Window capture: **Windows.Graphics.Capture (WGC)** — per-window GPU capture, DXGI texture → HW
  encoder ideally zero-copy.
- Encoding: **runtime select** — HW (Media Foundation: NVENC / QSV / AMF) if available, else SW
  (openh264 / x264). Bring-up first with **RAW_BGRA** (uncompressed) to get first pixels.
- Transport: **QUIC preferred + TCP fallback** (`quinn`). QUIC avoids TCP HoL on remote/lossy
  links. TCP is wired first (loopback VM has near-zero net latency).
- The bounds-checked codec (`Decode`/`Encode` over `ReadCursor`/`WriteCursor`, in `oxrdp-pdu`) is
  reused by the new protocol crate `oxproto`.
- **Agent runs in the interactive user session**, started by a logon Scheduled Task — *not* as a
  service. Session 0 cannot capture windows or inject input. Full reasoning and deployment plan:
  [`design/agent-runtime.md`](design/agent-runtime.md).
- **Security is part of the protocol, not a later layer**: mandatory TLS, a pinned agent
  certificate, and an auth token in `ClientHello`. The agent must not open a listener before
  these are wired — see [`../SECURITY.md`](../SECURITY.md).

## 4. Crate map (`crates/`)

Active (new direction):
- `oxproto` — the protocol: chunk framing (`envelope`, now bounding pre-auth reassembly state to
  64 pending channels / 64 MiB total — that state is allocated before the handshake), the
  message registry and all bodies (`message/{control,window,input}.rs`), wire primitives
  (`wire`). Implements [`design/OXPROTO.md`](design/OXPROTO.md). ✅ 28 unit + 6 robustness tests,
  plus `fuzz/` (`message`, `reassembly`).
- `oxtransport` — async chunk IO over any tokio stream; delegates reassembly to
  `oxproto::Reassembler`. ✅ 5 tests including an interleaved-channel head-of-line test.
- `oxsec` — TLS identity, pinning and token verification for the agent link: a self-signed
  identity persisted on first run, an SPKI-pin `ServerCertVerifier` used by the client (the pin
  authenticates, not the hostname), and a constant-time token comparison. Not the old
  `oxrdp-crypto::TofuVerifier`. ✅ 7 tests.
- `oxagent` — the Windows guest agent. **No longer a skeleton**: key/value config loading (a
  wildcard bind address is refused outright), an auth-gated handshake, a per-window frame-pacing
  budget (drop the oldest unacked frame rather than queue — queueing is the bufferbloat this
  project exists to avoid), a window registry (protocol ids are never reused within a session),
  and `serve.rs` — the session driver that streams windows over TLS with flow control. The
  platform sits behind a `WindowSource` trait, so the driver itself is unit-tested on Linux; only
  the trait implementation (WGC capture, Win32 enumeration) is Windows-only. Cross-compiles; not
  yet run inside a guest. ✅ 33 tests.
- `oxclient` — the Linux client session: handshake, feature negotiation, ping/pong housekeeping,
  and a `ClientEvent` stream (✅ 3 tests, `session.rs`), plus a `WindowModel` that turns that
  stream into an ordered, backend-independent list of instructions — create/retitle/restack —
  without retaining frame pixels (✅ 9 tests, `model.rs`). The `oxclient` binary is a bring-up CLI
  that connects over pinned TLS and prints the event stream, acking frames; it renders nothing
  (✅ 6 tests, `main.rs`). No display backend is wired in yet — that is P2b.

The client display/render architecture is decided but not yet built — see
[`design/client-display.md`](design/client-display.md). `oxrdp-display`, `oxrdp-render` and
`oxrdp-input` are **slated for deletion** (empty, pre-pivot skeletons); they are replaced by a
new `oxdisplay` crate (winit + an x11rb sidecar + a CPU `softbuffer` presenter, created at P2b)
and a new `oxrender` crate (`wgpu` + VA-API H.264 decode, created at P5, not before). `oxrdp-crypto`
holds the old TLS glue — its `TofuVerifier` accepts any certificate and **must not** be reused
for the agent connection; `oxsec` is its replacement.

Shelved (RDP-specific, kept in git): `oxrdp-pdu`'s RDP PDUs — **but its `codec`/`cursor`/`error`
are the reused base for `oxproto`** — plus `oxrdp-core`, `oxrdp-io`, `oxrdp-cli` (the RDP client
driver that reached MCS channel join against real Windows).

## 5. Build, test, run

```bash
# Linux workspace (agent builds as a stub here)
cargo build --workspace
cargo test  --workspace                       # 179 tests
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check

# Cross-compile the Windows agent from Linux (mingw-w64 is installed; rustup target added)
cargo build -p oxagent --target x86_64-pc-windows-gnu
#   → target/x86_64-pc-windows-gnu/debug/oxagent.exe   (PE32+, run inside the Windows guest)

cargo clippy -p oxagent --target x86_64-pc-windows-gnu --all-targets -- -D warnings

# Run the agent (in the guest). token_path must already contain the shared secret; the TLS
# identity is generated and persisted on first run at cert_path/key_path. Default bind is
# 127.0.0.1:7644 — a wildcard address is refused at config-parse time.
oxagent.exe --print-pin                        # prints the SPKI pin the client must trust, exits
oxagent.exe --config oxagent.conf              # serves one authenticated session at a time

# Connect the bring-up client CLI from Linux — prints the event stream, acks frames, renders
# nothing (no display backend exists yet):
cargo run -p oxclient -- HOST:7644 --pin <spki-hex> --token-file token.txt

# Fuzz the protocol decoder and the reassembler (needs nightly; fuzz/ is its own workspace)
cargo +nightly fuzz run message
cargo +nightly fuzz run reassembly

# The shelved RDP client (still works, reaches MCS channel join):
cargo run -p oxrdp-cli -- 127.0.0.1:3390 WPX-User    # OXRDP_DEBUG=1 for phase/hex logging
```

## 6. Test endpoint (real Windows)

A **winpodx dockur/windows guest** provides a real Windows target: container `winpodx-windows`,
RDP mapped to **`127.0.0.1:3390`** (→ guest 3389), `/sec:tls`, default user `WPX-User`. For the
new protocol, the plan is to run `oxagent.exe` **inside** that guest and connect `oxclient` from
Linux. (The user also has a separate Windows VM available as an alternative target.)

## 7. External-model orchestration (keep Claude/Anthropic usage low)

The workflow that has worked all session:
1. **Claude/human authors** a precise per-file **spec** with the exact existing API to use and
   **authoritative test vectors** (byte-exact where correctness matters).
2. **Offload the implementation** to a local **ollama cloud model** via `dev/oxgen.sh`:
   ```bash
   ollama serve &                                        # if not running
   dev/oxgen.sh kimi-k2.7-code:cloud spec_foo.txt crates/x/src/foo.rs false
   dev/oxgen.sh glm-5.2:cloud        spec_bar.txt crates/y/src/bar.rs false
   ```
   Models: `kimi-k2.7-code:cloud` (code specialist), `glm-5.2:cloud`. Run several in parallel
   (background). These are a **small plan** — watch usage (the helper logs tokens to
   `.cloud-usage.tsv`). `kimi think=true` is slow → prefer `think=false` + background.
   `gpt-oss:20b-local` is a free unlimited local fallback.
3. **Gate objectively with `cargo`** (fmt / clippy -D / test) — the free, objective verifier. Fix
   the model's misses (common: wrong error-variant shapes, missing `ctx` args, lifetime on
   `Decode<'de>`, `ok_or_else`→`ok_or`). Then commit.
4. Intricate `unsafe` Windows COM/WGC/MF code: **author directly** (models hallucinate the
   windows-rs API); validate by cross-compile + running in the guest.

`ollama launch claude --model glm-5.2:cloud` can also drive a headless Claude Code with a cloud
model; not used in favor of the leaner `oxgen.sh` REST path.

## 8. Roadmap & next steps

```
✅ P0  oxproto v1 — framing, channels, 25 message types, size limits, robustness tests, fuzz targets
✅ P1a cross-compile pipeline + oxagent skeleton (+ CI job that builds/lints it)
✅ P1b window enumeration (filtered, DWM frame bounds, DPI-aware) + oxtransport
✅ P1c agent capture — WGC per-window BGRA frames (cross-compile-validated, not yet run in a guest)
✅ P2a client session — handshake, features, ping/pong, ClientEvent stream
✅ P1d agent serve — TLS (oxsec) + auth-gated handshake, window registry, per-window frame
      pacing/flow control, serve.rs session driver (unit-tested on Linux via WindowSource).
      Client connects end to end and models window state (WindowModel). Neither side has run
      against a real Windows guest yet.

▶  P2b CLIENT PRESENT — architecture decided, see design/client-display.md; **wgpu is not part
      of this milestone**. New `oxdisplay` crate: winit owns the event loop and one native
      window per remote window (X11 first; an x11rb sidecar sets WM_TRANSIENT_FOR, which winit
      does not expose); a CPU presenter on `softbuffer` blits FrameData(RAW_BGRA) — a bounded
      memcpy, no GPU device at all. `oxrdp-display`/`oxrdp-render`/`oxrdp-input` are deleted, not
      filled in. **First end-to-end pixels.** Bring-up target 800x600@30fps (~460 Mbit/s);
      RAW_BGRA does not scale past that.
      Before building it out: spike S1 (winit-X11 multi-toplevel identity + transient-for
      sidecar smoke test, ~1-2 days) — failure means a native x11rb backend behind the same
      `Presenter`/`DisplayWindow` trait seam, not a change to the seam itself.

   P3  Input round-trip: Linux input → PointerEvent/KeyEvent/TextInput → SendInput on the guest.
       Also WindowControl (close/resize/activate), or the native windows stay puppets.
   P4  Multi-window: SetWinEventHook instead of polling, z-order, icons, app identity → WM_CLASS.
       Spike S2 (winit-Wayland identity, transient-for, xdg-toplevel-icon) settles winit vs
       smithay-client-toolkit for the Wayland backend, needed before this milestone.
   P5  Media Foundation H.264 (replace RAW_BGRA) via a new `oxrender` crate — a `wgpu` GPU
       presenter (device built via `wgpu_hal` so DMA-BUF import can be enabled), VA-API decode,
       an NV12→RGB shader. Spike S3 (VA-API → wgpu upload path: DMA-BUF import vs
       vaDeriveImage/vaMapBuffer copy, run on the target GPU) settles the upload path and the
       NVIDIA decision before this is built. QUIC transport, clipboard/audio also land here.
   P6  Latency harness: the protocol already carries captured_us/encoded_us and FrameAck —
       build the measurement rig and publish numbers against FreeRDP. This is the claim the
       whole pivot rests on; it must be measured, not asserted.
```

**Before starting P2b**, read [`design/client-display.md`](design/client-display.md) in full —
it is normative for the `oxdisplay` seam (`DisplayCommand`/`DisplayEvent`/`Presenter` trait), the
X11-vs-Wayland geometry policy (§4), and cursor handling (§5), not just a decision summary.

## 9. Key technical learnings (don't relearn these)

- **Extended CS_CORE required (RDP era, but a real Windows fact):** modern Windows *silently
  drops* a Connect-Initial whose CS_CORE is only the 128-byte mandatory part / 8bpp. Real clients
  send the **216-byte extended CS_CORE** (highColorDepth / supportedColorDepths /
  earlyCapabilityFlags …). This was the bug that blocked the live handshake; fixing it made the
  full RDP connection sequence work against real Windows.
- **Cross-compile pipeline works:** `x86_64-pc-windows-gnu` + mingw-w64 (both installed) build
  windows-rs 0.58 including WGC + Media Foundation + Win32 from Linux. No in-guest toolchain
  needed. `HWND.0 as isize`, `EnumWindows` raw-pointer callback, `BOOL`/`TRUE` from
  `windows::Win32::Foundation` — all compile on the gnu target.
- **GCC Conference Create Request prefix** (RDP): the canonical `00 05 00 14 7c 00 01` (T.124 OID)
  + `00 08 00 10 00 01 c0 00` preamble + `Duca` H.221 key is byte-correct; the trailing `00`
  before `Duca` is the octet-string length. `connectPDU_len = userData_len + 14`.
- **Codec reuse:** `oxrdp-pdu`'s `Decode`/`Encode` + bounds-checked `ReadCursor`/`WriteCursor` +
  typed errors are protocol-agnostic — `oxproto` builds directly on them.
- **windows-rs 0.58 specifics** (cost real time to find): the pixel format constant is
  `DirectXPixelFormat::B8G8R8A8UIntNormalizedSrgb`, *not* `…UnormSrgb`;
  `IGraphicsCaptureItemInterop` is a COM struct, so obtain it with
  `windows::core::factory::<GraphicsCaptureItem, _>()` and pass it by reference rather than
  using it as a trait bound; `HWND` is constructed from `*mut c_void`.
- **The feature list in `oxagent/Cargo.toml` is load-bearing.** WGC needs `Foundation`,
  `Graphics_Capture`, `Graphics_DirectX_Direct3D11`, `Win32_Graphics_Direct3D`,
  `Win32_System_WinRT_Direct3D11`, `Win32_System_WinRT_Graphics_Capture` and
  `Win32_System_Com` together; a missing one fails as a confusing "not found" rather than a
  missing-feature error.
- **`Direct3D11CaptureFramePool::CreateFreeThreaded`** avoids needing a `DispatcherQueue` and a
  message pump on the capture thread — use it, not `Create`.

## 10. Git / GitHub

- Repo: `https://github.com/kernalix7/oxrdp` (public, MIT, author Kim DaeHyun / kernalix7). Branch
  `main`. CI = `.github/workflows/ci.yml` (fmt/clippy/test/audit; guards on `Cargo.toml`).
- **No AI-attribution in commits/PRs** (hard rule). Conventional Commits.
- The name `oxrdp` is now a misnomer (no RDP in the new direction) — **rename TBD**.
