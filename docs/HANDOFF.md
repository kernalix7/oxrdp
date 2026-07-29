# oxrdp — Continuation handoff

Snapshot for continuing this project in another tool. Everything needed to resume is here; the
full history is in git (`github.com/kernalix7/oxrdp`, branch `main`).

Last state: **input injection, H.264 decode, and a security-hardened agent, all landed and
partly validated on a real Windows guest.** Since the last version of this document ("P2b done —
first pixels"), the project gained: keyboard/pointer/window-control injection into the guest,
per-window flags (`HAS_FRAME`/resizable/topmost) validated against four real Windows apps, a
minimize-not-close-and-reopen fix, an H.264 decoder (client-side, behind a trait, feature-gated),
a fully worked-out H.264 wire format, two cancel-safety bugs (read and write) that were
corrupting live sessions, a security review that found and fixed a real unauthenticated DoS, a
client-side bug that was writing host screen coordinates into the guest desktop, decode moved off
the session thread for latency, and a CI pipeline whose `cargo-deny` gate turned out to have been
silently failing since it was written. See [§9 What's validated](#9-whats-validated-vs-tested)
for exactly which of these were checked against the live guest and which rest on tests alone —
that distinction mattered three separate times today.

## 1. TL;DR

- **The project pivoted** (2026-07-02) from "a better RDP *client*" to **replacing RDP itself**
  (RustDesk / Moonlight style): a **Windows guest agent** captures individual app windows and
  streams them to a **Linux client** over a **custom, low-latency protocol**.
- The earlier **RDP-client** stack was validated end-to-end against a real Windows guest (through
  MCS channel join) before the pivot. It is **shelved** but kept in git; its client shells and
  codec base are reused by `oxproto`.
- **The full loop now runs against a real Windows guest**: a guest PowerShell window is captured,
  streamed, decoded and presented live as a native Linux window (§9). Input flows back —
  keyboard, pointer, and window control (close/move/resize/activate/minimize) — though not every
  input path has independently been confirmed against the guest (§9). H.264 decode exists on the
  client; the guest-side encoder does not yet, so every live run today is still `RAW_BGRA`.
- **The protocol is fully specified**: [`design/OXPROTO.md`](design/OXPROTO.md) is the
  authoritative wire spec — 17 sections, byte-exact conformance vectors, an H.264 payload format
  (§9.1) precise enough that an independently-written encoder and decoder need no side channel to
  agree on a byte, and a `close_reason`/`error_code` registry that says what each code promises
  the receiver (retry-worthy vs. fatal), added specifically because a rejection once used the
  wrong one.
- **Security got an adversarial pass, not just a design review**: a private key written
  world-readable and a token-comparison timing issue were found and fixed; a real unauthenticated
  denial-of-service (one idle TCP connection could block the accept loop forever) was found,
  fixed, and the fix was measured against the live guest, not just unit-tested. Full record:
  [`design/AUDIT-2026-07.md`](design/AUDIT-2026-07.md) (see its 2026-07-29 status sweep) and the
  CHANGELOG's "Security" section.
- Build is green: `cargo test --workspace` = **hundreds of tests across the active crates**
  (`cargo test --workspace 2>&1 | grep "test result"` to get the current exact count — it moves
  every session; do not trust a number quoted here). `cargo clippy --workspace --all-targets --
  -D warnings` is clean, and the agent cross-compiles from Linux to `x86_64-pc-windows-gnu`
  (enforced in CI, along with `cargo test -p oxclient --no-default-features` and a `cargo-deny`
  gate that — see §7 — was not actually gating anything until 2026-07-29).
- A multi-agent **gap audit** (56 findings, `design/AUDIT-2026-07.md`) drove the original
  protocol redesign. It now carries a 2026-07-29 status sweep marking each finding fixed /
  partially fixed / open / stale — read that before assuming any individual finding still
  describes the repo.

## 2. Target architecture (post-pivot)

```
[Linux] oxclient  ──oxproto (TCP+TLS now, QUIC planned)──▶  [Windows guest] oxagent
  · decode (openh264 SW now; VA-API/wgpu at P5)                · enumerate app windows (Win32)
  · map each remote window → native Linux window (X11/Wayland) · capture per window (WGC)
  · capture Linux input → send; inject guest input events back · encode (RAW_BGRA now; MF H.264 in flight)
                                                                · inject input (SendInput)
```

The **hard part** (still true): per-app *seamless* windows = re-implementing RemoteApp's server
side in the agent (WGC per-window capture + window events + z-order + decoration). RDP/RemoteApp
gave this for free; the custom protocol has to build it, and most of the pieces above are now
built — the remaining gap is the H.264 *encoder* (in flight, see §8) and full z-order/multi-window
polish.

## 3. Locked decisions (agent architecture)

- Guest agent stack: **Rust + `windows-rs`** (0.58).
- Window capture: **Windows.Graphics.Capture (WGC)** — per-window GPU capture, row-pitch-aware
  readback, reused staging texture, frame-pool recreation on resize. **Validated against a real
  guest** (§9).
- Encoding: **runtime select** — Media Foundation HW/SW if available, else `openh264`. Bring-up is
  **RAW_BGRA**; the Media Foundation H.264 encoder is being written now (§8) — nothing negotiates
  H.264 on the wire yet, only `RAW_BGRA`.
- Transport: **TCP + TLS 1.3 now** (mandatory, `oxsec`); **QUIC planned**, not started.
- The bounds-checked codec (`Decode`/`Encode` over `ReadCursor`/`WriteCursor`, in `oxrdp-pdu`) is
  reused by `oxproto`.
- **Agent runs in the interactive user session**, started by dockur's OEM-folder provisioning +
  a logon Scheduled Task — *not* as a service. Full reasoning:
  [`design/agent-runtime.md`](design/agent-runtime.md). The actual guest-provisioning mechanism
  (OEM folder, since dockur exposes no exec API) is recorded, with its own verification status,
  in [`design/agent-deploy.md`](design/agent-deploy.md) — read the "Status" line at its top
  before trusting any claim in it.
- **Security is part of the protocol, not a later layer**: mandatory TLS, a pinned agent
  certificate (0600 permissions on the key file, Unix; Windows ACL hardening is a documented gap,
  see [`../SECURITY.md`](../SECURITY.md)), and a constant-time-compared auth token in
  `ClientHello`. The agent authenticates before touching capture or `source`/`sink` at all, and
  handshakes now run under a bounded pre-auth deadline and a bounded pre-auth connection count
  (§7) — a single unauthenticated TCP connection can no longer block every other client
  indefinitely, which it could until 2026-07-29.

## 4. Crate map (`crates/`)

Active (new direction):
- `oxproto` — the protocol: chunk framing (`envelope`, bounding pre-auth reassembly state to 64
  pending channels / 64 MiB total), the full message registry (handshake, window lifecycle,
  video + flow control, input, cursor, errors/close/liveness), wire primitives. Implements
  [`design/OXPROTO.md`](design/OXPROTO.md) exactly — where they disagree, the doc is the bug
  report. Has its own `tests/conformance.rs` (byte-exact wire fixtures, hand-written, never
  encoder-produced) and `tests/robustness.rs` (deterministic smoke-fuzz), plus `fuzz/` for
  coverage-guided cargo-fuzz targets (`message`, `reassembly`) — those run on a **daily scheduled
  CI workflow**, not on every push, since they need nightly and a useful run is slower than a PR
  gate should block on.
- `oxtransport` — async chunk IO over any tokio stream; delegates reassembly to
  `oxproto::Reassembler`. Both reads (`ChunkReader`) and writes (`ChunkWriter`) are cancel-safe —
  progress lives in the caller's state, not in a future that a `tokio::select!` branch can drop
  mid-chunk. This was **not** always true: a real cancel-safety bug on the read side desynced a
  live session (§9), and an audit found the same bug class existed on the write side before it
  had ever bitten anyone.
- `oxsec` — TLS identity, pinning and token verification for the agent link: a self-signed
  identity persisted on first run (0600 permissions), an SPKI-pin `ServerCertVerifier`, and a
  constant-time token comparison whose loop bound is now the *server's* fixed token length, not
  whatever length an unauthenticated peer chooses to send (a real, if narrow, timing-safety gap
  found and fixed 2026-07-28).
- `oxagent` — the Windows guest agent. Config loading (wildcard bind refused outright), an
  auth-gated handshake now running under a pre-auth deadline + bounded pre-auth connection count
  with each connection on its own task (so a panic anywhere in TLS/handshake/drive/injection
  takes down one session, not the process), a window registry (ids never reused within a
  session), per-window frame pacing (drop the oldest unacked frame rather than queue), real
  window flags (`resizable`/`has_frame`/`topmost`/`minimized`/`maximized`, with the `HAS_FRAME`
  heuristic validated against four real Windows apps — see §9), a minimize fix (a minimized
  window used to vanish and reopen as a new one; it now reports `WindowState` and stays open),
  and `InputSink` — keyboard by scancode, pointer normalized to the virtual desktop, window
  control (close/activate/minimize/maximize/restore/move/resize) via `SendInput`/`SetWindowPos`.
  The platform sits behind `WindowSource`/`InputSink` traits, so the driver is unit-tested on
  Linux; only the trait implementations (WGC capture, Win32 enumeration, `SendInput`) are
  Windows-only, cross-compile-validated, and — for capture and window flags — validated against a
  real guest. **In flight right now** (uncommitted in the working tree as of this writing): a
  Media Foundation H.264 encoder (`encode.rs`, `h264.rs`, `nv12.rs`) — do not assume it exists
  until it lands.
- `oxclient` — the Linux client. `session.rs` performs the handshake (feature negotiation,
  ping/pong), and reads are cancel-safe (`ChunkReader`) the same way `oxtransport` is. `model.rs`
  turns the event stream into an ordered, backend-independent `WindowModel`. `geometry.rs` is a
  displacement-based sync that stops the window manager's own placement/resize events from being
  echoed back to the guest as if the user had dragged something — a real bug (host coordinates
  landing in guest space) that shipped and was fixed 2026-07-28; a narrower follow-on gap (a
  window manager that reports placement in more than one step spanning the 750ms settle window
  can still produce a phantom move) was found in review and is routed to this crate's owner, not
  yet fixed. `decode/` holds a `Decoder` trait, a `PassthroughDecoder` for `RAW_BGRA`, and
  `H264Decoder` (via `openh264`, behind the default-on `h264` feature — CI also runs
  `--no-default-features` so the validated raw path can't silently rot). Decode now runs on **one
  worker thread per window**, off the session task, so decode time is no longer charged to input
  latency; a full window's worth of frames backs up in a bounded per-window queue and applies
  backpressure (stops reading the network) rather than dropping — dropping an inter frame client
  side corrupts the stream until the next agent-chosen keyframe, and there's no way to ask the
  agent for one early. `oxdisplay` (winit + softbuffer CPU presenter) is the display backend;
  `oxclient`'s `main.rs` wires session, decode pipeline, geometry sync and display together and
  is the actual end-to-end binary now, not just a bring-up CLI.

The client display/render architecture is decided — see
[`design/client-display.md`](design/client-display.md) and
[`design/window-decorations.md`](design/window-decorations.md) (the `HAS_FRAME` decoration
policy, including why `WindowState.flags` had to gain real meaning mid-session — entering full
screen changes `HAS_FRAME`, and a client that didn't re-learn that would keep making the same
"two title bars" mistake, just later). `oxrdp-display`, `oxrdp-render` and `oxrdp-input` remain
empty pre-pivot skeletons, still slated for deletion once `oxrender` (the `wgpu` + VA-API
presenter, P5) actually needs to exist.

Shelved (RDP-specific, kept in git): `oxrdp-pdu`'s RDP PDUs — **but its `codec`/`cursor`/`error`
are the reused base for `oxproto`** — plus `oxrdp-core`, `oxrdp-io`, `oxrdp-cli` (the RDP client
driver that reached MCS channel join against real Windows). All shelved crates are now marked
`publish = false` (a mechanical cleanup that also, incidentally, is what finally lets
`cargo-deny`'s wildcard check mean something real — see §7).

## 5. Build, test, run

```bash
# Linux workspace (agent builds as a stub here)
cargo build --workspace
cargo test  --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check

# The configuration that keeps the validated RAW_BGRA path alive without openh264/its C build
cargo test -p oxclient --no-default-features

# Cross-compile the Windows agent from Linux (mingw-w64 is installed; rustup target added —
# rust-toolchain.toml pins both so this works on a fresh checkout with no extra setup)
cargo build -p oxagent --target x86_64-pc-windows-gnu
cargo clippy -p oxagent --target x86_64-pc-windows-gnu --all-targets -- -D warnings

# Run the agent (in the guest). token_path must already contain the shared secret; the TLS
# identity is generated and persisted on first run at cert_path/key_path (0600 on Unix; Windows
# ACL hardening is a known, documented gap). Default bind is 127.0.0.1:7644 — wildcard refused
# unless allow_wildcard_bind = true (the VM-behind-a-port-forward case).
oxagent.exe --print-pin
oxagent.exe --config oxagent.conf

# Connect the client — full end-to-end path: handshake, decode, present, inject input back
cargo run -p oxclient -- HOST:7644 --pin <spki-hex> --token-file token.txt
cargo run -p oxclient -- HOST:7644 --pin <spki-hex> --token-file token.txt --headless   # bring-up path, no display

# Fuzz the protocol decoder (needs nightly; fuzz/ is its own workspace). Runs daily in CI
# (.github/workflows/fuzz.yml); run manually after touching envelope.rs or the message decoders:
cargo +nightly fuzz run message
cargo +nightly fuzz run reassembly

# The shelved RDP client (still works, reaches MCS channel join):
cargo run -p oxrdp-cli -- 127.0.0.1:3390 WPX-User    # OXRDP_DEBUG=1 for phase/hex logging
```

CI (`.github/workflows/ci.yml`) runs all of the above except the fuzz targets, plus `cargo audit`
and `cargo deny check bans licenses sources` — the latter genuinely gates now; it did not until
2026-07-29 (§7).

## 6. Test endpoint (real Windows)

oxrdp now has **its own dedicated Windows guest**, provisioned by `dev/vm/oxrdp-windows.sh` via
dockur/windows with an OEM-folder install script (`dev/vm/oem/`), not borrowed winpodx's guest —
see [`design/agent-deploy.md`](design/agent-deploy.md) for exactly what that script does and its
own verification status. `dev/vm/oxrdp-windows.sh status` performs a real TLS handshake against
the agent and prints its SPKI pin, checked against the agent's own `--print-pin`, rather than
guessing liveness from an RDP probe. A batch file on the guest's shared folder
(`dev/vm/oem/restart-agent.bat` — see commit `4f95dd3`) restarts the agent in place without
requiring a quoted PowerShell one-liner typed through a synthetic-input RDP session, which is
still, today, the only channel *into* the guest during bring-up before oxagent's own input path is
trusted for that job.

## 7. CI and supply-chain gates

`.github/workflows/ci.yml` runs `lint` (fmt+clippy), `test` (matrix ubuntu-latest /
ubuntu-24.04-arm, including `--no-default-features` for `oxclient`), `agent-windows` (mingw-w64
cross-build + clippy on `x86_64-pc-windows-gnu`), `audit` (RUSTSEC advisories), and `deny`
(`cargo-deny` bans/licenses/sources) — all with `Swatinem/rust-cache` so `openh264`'s vendored C
build doesn't dominate every cold run. `.github/workflows/fuzz.yml` runs both cargo-fuzz targets
daily plus on manual dispatch, deliberately not on every push (needs nightly; a useful run is
slower than a PR should wait on).

**The `deny` job had been failing since the day it was added, and nobody had run it.**
Every workspace crate depends on its siblings by `path = "../..."` with no `version =`, which
`cargo-deny`'s wildcard check flags exactly like a genuine unpinned crates.io dependency; its
`allow-wildcard-paths` exemption only covers crates marked `publish = false`, which none were.
Found by actually running `cargo deny check bans licenses sources` locally rather than trusting
the CI job that claimed to run it — fixed in two steps: `wildcards` downgraded to `warn` with
`allow-wildcard-paths = true` set in anticipation (2026-07-29), then every workspace crate marked
`publish = false` so `wildcards` could go back to `deny` for real, verified by reverting one
crate's marker and confirming the check actually fails. If you're reading this and `deny.toml`
still says `wildcards = "warn"`, that mechanical cleanup didn't land yet — check `deny.toml`'s own
comments for the current story.

## 8. Roadmap & next steps

```
✅ P0  oxproto v1 — framing, channels, full message registry, size limits, robustness tests, fuzz
✅ P1  agent skeleton, cross-compile pipeline, window enumeration, async transport
✅ P1c agent capture — WGC per-window BGRA frames, VALIDATED against a real guest (§9)
✅ P1d agent serve — TLS + auth-gated handshake, window registry, frame pacing/flow control,
      DoS-hardened (bounded pre-auth deadline + connection count, per-connection task isolation)
✅ P2  client session — handshake, features, ping/pong, cancel-safe reads and writes
✅ P2b client present — oxdisplay (winit + softbuffer), first end-to-end pixels VALIDATED against
      a real guest (§9): a live guest PowerShell window as a native Linux window, RAW_BGRA
✅ P3  input round-trip — keyboard (scancode), pointer, window control (close/move/resize/
      activate/minimize/maximize) via SendInput/SetWindowPos. WindowControl exercised against
      the real guest (indirectly — see §9); keyboard/pointer injection not independently
      confirmed against the guest yet.
▶  P4  Multi-window polish: z-order sync, icons, further HAS_FRAME edge cases, the
      cross-channel-ordering geometry gap noted in §4 (crates/oxclient — routed, not yet fixed).
▶  P5  Media Foundation H.264 encoder — IN FLIGHT NOW (uncommitted: crates/oxagent/src/
      {encode,h264,nv12}.rs). Client-side H.264 decode already landed (2026-07-28), behind the
      default-on `h264` feature, validated only via unit tests against openh264's own encoder
      output — not yet against a real agent-encoded stream, since the agent doesn't encode H.264
      yet. Once the encoder lands: negotiate H.264 end-to-end against the real guest and record
      the result here with the same rigor as P1c/P2b/P3 got.
   P5b SIMD for the YUV→BGRA conversion — flagged as the next perf target: measured at 75-87% of
      client-side decode cost at the sizes where the measurement is clean (6f005ba). Not started.
   P6  Latency harness: the protocol now carries every timestamp needed (captured_us/encoded_us/
      decoded_us/presented_us, Ping/Pong for clock offset) but nothing has assembled them into a
      measured number yet, and there is still no recorded FreeRDP baseline on the same guest.
      This is the claim the whole pivot rests on; it remains unmeasured.
      QUIC transport, clipboard/audio also land around here.
```

## 9. What's validated vs. tested

This distinction has been the single most valuable thing in this project's history so far: three
separate bugs (a WGC pixel-format mismatch, a cancel-safety defect that silently corrupted a
live stream, host screen coordinates leaking into guest window positions) were invisible to a
fully green test suite and were only ever going to be found by actually running the thing against
the real guest. Treat everything below as the current ledger, and update it — don't let it drift
the way the old "90 tests, clippy clean" line did.

**Validated against the live guest** (not just unit-tested):
- WGC per-window capture — first end-to-end pixels, a live guest PowerShell window as a native
  Linux window (1115×628 RAW_BGRA, ~21 fps, ~470 Mbit/s for one window).
- The `HAS_FRAME` decoration heuristic — four real Windows apps (`regedit.exe`, `charmap.exe`,
  Windows Terminal, Windows 11 Notepad), all four correct.
- The minimize fix — minimizing Notepad now reports `window state: id=2 state=1` with no
  `WindowClosed`, confirmed via a real restart-and-observe cycle against the guest.
- Read- and write-side cancel-safety — "a guest PowerShell window now appears as a native
  1115x628 Linux window showing live, updating content" (the windowed client used to die a few
  seconds in).
- The unauthenticated-DoS fix — measured on the guest: a silent peer is closed after the 20s
  deadline, and a legitimate handshake completes immediately while three silent peers are held
  (before the fix, it never completed at all while one was held).
- WindowControl (at least move/resize) reaching the guest and having real effect — this is *how*
  the host-coordinates-in-guest-space bug (`dcc8c64`) was discovered: `regedit` and `charmap`
  were observed actually resized and moved on the real guest to exactly the buggy positions.

**Implemented and unit-tested, not yet independently confirmed against the live guest:**
- Keyboard and pointer injection specifically (`SendInput` for keys/clicks) — P3 landed; I found
  no commit recording a dedicated "typed into a real guest app and it worked" check the way
  HAS_FRAME and the minimize fix got one. Worth doing before trusting it further.
- H.264 decode (`H264Decoder`) — extensively unit-tested against clips produced by `openh264`'s
  own encoder (round-trip self-test), and confirmed *not to break the RAW_BGRA path* when
  compiled in and run against the live guest. The decoder has never decoded a frame the agent
  actually encoded, because the agent doesn't encode H.264 yet.
- The geometry-fix's wiring — the *policy* (`GeometrySync`) is a pure unit tested exhaustively;
  whether winit's `Moved`/`Resized` events actually reach it end to end was verified by reading
  the code, not by a scripted X session, per the commit's own note.
- Decode-off-session-task performance numbers (4.4/15.5/57 ms at 800x600/1080p/4K) — measured on
  a release build locally; not confirmed as the guest's real observed decode cost end to end.

**Known open gap, not yet fixed** (routed to the owning crate, per the 2026-07-29 review):
- `crates/oxclient/src/geometry.rs`: a window manager that reports a window's placement or size
  in **two or more steps** where the transition straddles the fixed 750ms settle deadline still
  produces a phantom move/resize — the "first observation is an anchor" safety property only
  covers the very first report after creation, not the first report *after settling ends*. Ruling
  from review: fix by making any report observed *while* settling not become the anchor either,
  so the first post-settling report is always fresh. Not yet implemented.

**Not started at all:** Media Foundation H.264 encode (in flight, §8), the latency harness (P6),
QUIC, clipboard, audio, `wgpu`/VA-API presentation (still softbuffer/CPU).

### Input: works end to end, targeting fixed (measured 2026-07-29)

Bisected against the live guest rather than inferred, and the earlier entry saying input was
merely "unverified" understated what is now known:

- **Keyboard injection works end to end.** Typing `abc` in the client produced scancodes
  `0x1e/0x30/0x2e` with correct press and release pairs on the wire, and the characters arrived
  in the guest's Notepad — its title became `*abc - Notepad`. A later run typed `ZZZTEST` and it
  arrived as uppercase, so scancode and modifier handling are sound.
- **The client's X11 window genuinely holds keyboard focus** — window id, `getactivewindow`,
  `_NET_ACTIVE_WINDOW` and `getwindowfocus` all agree, so no `KeyboardInput` events are being
  lost before the protocol.
- **Window targeting is the defect.** The agent resolves a `PointerEvent`'s `window_id` to the
  right `HWND` and converts to absolute screen coordinates correctly, but the injected click
  lands on whatever guest window is **topmost at that position** — the addressing is discarded
  at the last step. `ZZZTEST` went to a PowerShell window stacked over Notepad. Keys then follow
  guest OS focus to the wrong window. It is intermittent in exactly the way that implies:
  whether a click reaches its target depends on the guest's z-order at that instant.
- Every one of those failures was **silent**. `SetForegroundWindow`'s result was discarded, and
  Windows' anti-focus-stealing heuristic disqualifies a background process from raising a window
  — so it was failing on every attempt with nothing to show for it.

**Fixed and re-measured.** The agent now raises the addressed window before injecting, falling
back to attaching its input state to the current foreground thread when the plain call is
refused, and confirms the result rather than assuming it. A click whose raise cannot be
confirmed is withheld rather than injected somewhere else — the same doctrine already applied to
input for an unknown window id, and the PowerShell case is why it matters: a misdirected click
can land somewhere more privileged than the user intended.

The intermittency had its own cause: `last_focused` was recorded before checking whether the
raise worked, so one silent failure convinced the agent it had already focused that window and
it never retried.

Verified against the guest in the scenario that previously failed — a Notepad window with two
Terminal windows stacked over it. Typing after a click now lands in Notepad: its title went from
`*abcabc - Notepad` to `*abcabczz - Notepad`.

## 10. Key technical learnings (don't relearn these)

- **Cancel-safety is not optional once anything uses `tokio::select!`.** `read_exact`/`write_all`
  keep their progress in the future's own local state; a future dropped mid-chunk (the losing
  branch of a `select!`) takes that progress with it, and the next read/write resumes in the
  middle of a chunk. The fix pattern (`ChunkReader`/`ChunkWriter` in `oxtransport`) keeps progress
  in `&mut self` instead. This bit a real session; check any new `select!` against it.
- **A wire-spec ambiguity is a bug whether or not code has shipped against it.** §9.1's
  AUD-vs-SPS-first contradiction and the missing `SESSION_BUSY`/`close_reason` distinction both
  cost real design time to resolve *after* two implementations were already being built against
  the ambiguous version. Pin field semantics, ordering, and what a code promises the receiver
  before handing a spec to a second implementer, not after.
- **`WindowState.flags` now means the same thing `WindowOpened.flags` does**, and is the sole
  vehicle for any change to `state` or `flags` after a window opens — always the complete current
  value, never a delta, and a `HAS_FRAME` change must be followed by a fresh `WindowGeometry`
  because that bit decides which coordinate space geometry is in.
- **Extended CS_CORE required (RDP era, but a real Windows fact):** modern Windows *silently
  drops* a Connect-Initial whose CS_CORE is only the 128-byte mandatory part / 8bpp; real clients
  send the 216-byte extended CS_CORE.
- **Cross-compile pipeline works and is now CI-enforced**, not just a local habit:
  `x86_64-pc-windows-gnu` + mingw-w64 build `windows-rs` 0.58 including WGC + Media Foundation +
  Win32 from Linux, and CI now cross-builds and clippy-lints the agent on every push.
- **`SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)` and DWM extended frame bounds, not
  `GetWindowRect`, are load-bearing from the very first capture** — `GetWindowRect` includes DWM's
  invisible resize border and is DPI-virtualized without the awareness call; the agent now does
  both correctly, but this is exactly the class of bug that looks like a renderer problem.
- **`SendInput` has no per-window target** — it always goes to whatever has Win32 focus. The
  agent brings a window to the foreground only on an explicit user action (a newly-pressed button
  targeting a different window, or `WindowControl::ACTIVATE`), never on passive motion, so
  hovering one window while typing into another behaves correctly without per-event focus
  thrashing.
- **A client cannot trust host window-manager placement/resize events as user intent.** Creating
  a native window, or applying the guest's own move, both provoke WM events indistinguishable
  from a real drag; the fix sends the guest a *displacement*, never a position, which needs two
  local observations to produce one move — making an accidental echo structurally impossible
  rather than merely unlikely. (Narrower residual gap: see §9.)
- **`cargo-deny`'s wildcard check does not understand an intra-workspace `path` dependency
  without `publish = false`** — a gate that looks green because it's silently never been run is
  worse than an honest red one; always execute the tool the CI job claims to run, don't just read
  the job.

## 11. Git / GitHub

- Repo: `https://github.com/kernalix7/oxrdp` (public, MIT, author Kim DaeHyun / kernalix7). Branch
  `main`. CI = `.github/workflows/{ci,fuzz}.yml`.
- **No AI-attribution in commits/PRs** (hard rule). Conventional Commits.
- The name `oxrdp` is now a misnomer (no RDP in the new direction). README.md says the rename is
  "deliberately deferred until the first end-to-end milestone" — that milestone happened
  2026-07-28 (§9). The rename question is therefore live again, not settled either way.
