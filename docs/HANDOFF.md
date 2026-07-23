# oxrdp — Continuation handoff

Snapshot for continuing this project in another tool (e.g. opencode). Everything needed to
resume is here; the full history is in git (`github.com/kernalix7/oxrdp`, branch `main`).

Last state: **P1 in progress** — see [Roadmap & next steps](#roadmap--next-steps).
Latest commit at handoff: `5e1cd91` (+ this doc and the `oxclient`/`dev` additions).

---

## 1. TL;DR

- **The project pivoted** (2026-07-02) from "a better RDP *client*" to **replacing RDP itself**
  (RustDesk / Moonlight style): a **Windows guest agent** captures individual app windows and
  streams them to a **Linux client** over a **custom, low-latency protocol**. Rationale: RDP has
  structural latency limits (TCP head-of-line blocking by default, bandwidth-optimized buffering,
  general-purpose overhead) a purpose-built protocol can beat.
- The earlier **RDP-client** stack was built and **validated end-to-end against a real Windows
  guest** (through MCS channel join) before the pivot. It is **shelved** but kept in git; its
  client shells + codec base are reused.
- Build is green: `cargo test --workspace` = **90 tests**, `cargo clippy --workspace -- -D warnings`
  clean, and the Windows agent **cross-compiles from Linux** to `x86_64-pc-windows-gnu`.

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

## 4. Crate map (`crates/`)

Active (new direction):
- `oxproto` — **custom protocol wire messages** (sans-io). `Message` envelope (type u8 + len u32 LE
  + payload) with `ClientHello / ServerHello / WindowCreated / WindowClosed / FrameData /
  PointerEvent`. Re-exports `decode`/`encode_vec`. Built on `oxrdp-pdu`'s codec. ✅ 7 tests.
- `oxtransport` — async framing of `oxproto` messages over any tokio stream
  (`read_message_bytes` / `write_message`, 64 MiB guard). ✅ 2 tests.
- `oxagent` — **Windows guest agent** binary. Windows deps are `cfg(windows)`-gated so the
  workspace builds it as a stub on Linux (CI green) and it **cross-compiles** to windows-gnu.
  Done: Win32 window enumeration (`EnumWindows` → handle/title/geometry, `src/win.rs`). ⏳ WGC
  capture + encode + serve next.
- `oxclient` — **Linux client session** (in progress). `session.rs` currently defines
  `ClientEvent` + a `ClientSession` params struct; the `connect`/`next_event` loop is TODO (P2).

Reused shells (from the RDP era, carry over): `oxrdp-crypto` (TLS), `oxrdp-io` (tokio transport),
`oxrdp-render` (wgpu + VA-API decode — still a skeleton), `oxrdp-display` (X11/Wayland window
mapping — skeleton), `oxrdp-input` (skeleton).

Shelved (RDP-specific, kept in git, not on the new path): `oxrdp-pdu`'s RDP PDUs (nego, mcs, gcc,
connect_initial/response, capability, finalize, …) — **but its `codec`/`cursor`/`error` are the
reused base**; `oxrdp-core` (RDP connection state machine); `oxrdp-cli` + `oxrdp-io::connect`
(the RDP client driver, which reached MCS channel join against real Windows).

## 5. Build, test, run

```bash
# Linux workspace (agent builds as a stub here)
cargo build --workspace
cargo test  --workspace                       # 90 tests
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check

# Cross-compile the Windows agent from Linux (mingw-w64 is installed; rustup target added)
cargo build -p oxagent --target x86_64-pc-windows-gnu
#   → target/x86_64-pc-windows-gnu/debug/oxagent.exe   (PE32+, run inside the Windows guest)

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
✅ P0  oxproto — protocol messages
✅ P1a cross-compile pipeline + oxagent skeleton (windows-gnu)
✅ P1b Win32 window enumeration + oxtransport (async message framing)
▶  P1c AGENT capture: WGC capture ONE window (D3D11 device → GraphicsCaptureItem from HWND via
        IGraphicsCaptureItemInterop::CreateForWindow → Direct3D11CaptureFramePool → copy to a
        CPU-readable staging texture → BGRA bytes). Author directly; cross-compile to validate.
   P1d AGENT serve: TcpListener → oxproto handshake → send WindowCreated + FrameData(RAW_BGRA)
        for the captured window, using oxtransport. (Cross-platform; can go to a model.)
   P2  CLIENT session (oxclient): implement ClientSession over a tokio stream using oxtransport —
        connect → ClientHello → ServerHello → next_event() → ClientEvent. (Good model task; test
        with a tokio duplex + canned server messages, like the old connector tests.)
   P2b CLIENT present: map WindowOpened → a native Linux window (oxrdp-display) and blit
        FrameData(RAW_BGRA) via oxrdp-render (wgpu). First end-to-end PIXELS.
   P3  Input round-trip (Linux input → PointerEvent/KeyEvent → agent injects via SendInput).
   P4  Multi-window: enumerate + per-window capture + window events + z-order sync.
   P5  Media Foundation H.264 (replace RAW_BGRA); QUIC transport (quinn); clipboard/audio.
```

**Immediate next action (P1c):** write `crates/oxagent/src/win/capture.rs` (or `capture.rs`) that,
given an HWND from `enumerate_windows()`, captures a single BGRA frame via WGC. Cross-compile
after each change. Then P1d serve + P2 client to get first pixels end-to-end (run `oxagent.exe` in
the winpodx guest, `oxclient` on Linux).

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

## 10. Git / GitHub

- Repo: `https://github.com/kernalix7/oxrdp` (public, MIT, author Kim DaeHyun / kernalix7). Branch
  `main`. CI = `.github/workflows/ci.yml` (fmt/clippy/test/audit; guards on `Cargo.toml`).
- **No AI-attribution in commits/PRs** (hard rule). Conventional Commits.
- The name `oxrdp` is now a misnomer (no RDP in the new direction) — **rename TBD**.
