# Changelog

**English** | [한국어](docs/CHANGELOG.ko.md)

All notable changes to oxrdp are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims to follow
[Semantic Versioning](https://semver.org/) once releases begin.

## [Unreleased]

### H.264 encoding, measured on the guest (2026-07-29)

The agent now encodes with Media Foundation, and `codec=2` negotiates end to end against the
real guest. The number this work existed to change:

| | RAW_BGRA | H.264 |
|---|---|---|
| keyframe, 596x623 window | 1,485,232 B | 27,262 B |
| delta frame, same window | 1,485,232 B | **236 B** |

Every frame was a full uncompressed buffer before, because there was no encoder — a single
window cost roughly 470 Mbit/s. A delta frame is now about 6,300 times smaller than the buffer
it replaces, and even a keyframe is 54 times smaller.

Written to §9.1 rather than to Media Foundation's defaults, which disagree with it: parameter
sets are re-emitted on every keyframe whether or not the transform included them, the KEYFRAME
flag is set from the NAL type that actually came out rather than from what was requested, and
B-frames are disabled through ICodecAPI rather than assumed — §12's flow control drops the
oldest unacknowledged frame, which is unsound the moment a later frame can reference a skipped
one. If no encoder is available the agent never offers H.264 and sessions run on RAW_BGRA
exactly as before.

Three things are documented as unverified in the encoder's own module doc rather than only in a
report: whether `MF_TRANSFORM_ASYNC_UNLOCK` really drives this guest's hardware encoder
synchronously, whether the driver honours the `ICodecAPI` properties rather than silently
ignoring them, and the COM ownership handling around `MFTEnumEx`.

**Follow-up the same day: the stream was out of spec, and finding out took three rounds of
instrumentation.** The client's decoder rejected exactly one frame in every thirty — the seventh
of each GOP. Chasing it produced the following, in order:

- Nothing in the workspace installed a logger, so every `log::warn!` in `oxdisplay` had been
  going to a no-op sink. The stream had been dropping a frame a second while looking healthy.
- openh264's `Native:16384` is `dsOutOfMemory`, not a bitstream error — which falsified the
  parameter-set theory the first round was built on.
- Raw capture of fifteen rejected access units showed every one was an AUD plus a single
  non-IDR slice with `nal_ref_idc = 0`: non-reference pictures, which is what B-frames are.
- Parsing the SPS on both sides — two independently written parsers agreeing — showed Media
  Foundation encoding at **Main profile**, so `CODECAPI_AVEncMPVDefaultBPictureCount = 0` had
  been silently ignored, as had the GOP size, and as had the profile property itself, which
  did not even echo back.

That made it a spec violation rather than a decoder incompatibility: §9.1 forbids B-frames
because §12's flow control drops the oldest unacknowledged frame, which is unsound the moment a
later frame can reference a skipped one. The stream was wrong whether or not anything could
decode it.

Fixed by constraining the profile through the **output media type** rather than a property. A
media type is negotiated — `SetOutputType` either accepts Constrained Baseline or fails — while
a property is advisory and this encoder had disregarded three of them. Verified on the guest, both sides
agreeing: `profile_idc=66, constraints=0xc0`, and zero rejections and zero resynchronisations
where the same setup previously produced twelve to twenty-eight rejections. The agent's own NAL
log carries the direct proof that B-frames are gone — every delta slice now reports
`ref_idc=3`, where the rejected frames had been `ref_idc=0` non-reference pictures.

### Latency, measured for the first time (2026-07-29)

This project exists because RDP has structural latency limits a purpose-built protocol can beat.
That claim had never been checked. It has now, against the live guest, release builds on both
sides, 426 frames presented:

```
capture->encode  p50   5,845 us
encode->arrival  p50   3,419 us   p95  46,347   p99  50,347   (+/- clock error)
arrival->decode  p50   5,863 us
decode->present  p50   1,501 us
client total     p50   7,299 us
END TO END       p50  18,205 us   p95  60,025   p99  66,211
```

**18 ms median, capture to present** — through a VM port forward, with a software H.264 encoder
and a software decoder, and no GPU anywhere in the path. The tail is the finding: p95 and p99 are
three times the median, and almost all of it sits in `encode->arrival`, the transport hop, which
has no business costing 46 ms over loopback. That is where to look next, not at the codec.

Three things this measurement establishes beyond the numbers:

- **The client sent no pings and discarded every pong.** `ClockSync` had never had any data, so
  no offset could ever have been computed. Wiring it up was not enough; the exchange it depends
  on did not exist.
- **Build profile dominates everything.** The same measurement on debug builds reported
  `arrival->decode` at 75 ms where release reports 5.9 ms, and put the bottleneck in a completely
  different stage. A latency figure without its build profile is not a figure.
- **Only one of the four stages crosses clocks.** Capture-to-encode, arrival-to-decode and
  decode-to-present are each differences between two readings of the same clock, so they are
  exact regardless of how far apart the two ends' clocks are; only `encode->arrival` and the
  total inherit the offset error, and the report prints the round trip so a reader can size it.

It measures **capture to present**, and says so in its own header. The guest's compositor before
capture and the local display server after present are outside every process involved; a real
glass-to-glass figure needs a camera pointed at two screens.

### Security (2026-07-28)

An adversarial review of the agent's network-facing surface, prompted by input injection
landing — before it, an authentication weakness leaked pixels; now it hands an attacker
synthetic keystrokes in an interactive Administrator session.

Fixed:

- **The agent's TLS private key was written world-readable** (0644 under a default umask). Any
  local account could read it and impersonate the agent to any client trusting its pin: pinning
  validates a public-key hash and TLS proves possession of the matching private key. TLS 1.3's
  forward secrecy means this was never a decrypt-old-traffic bug — it was impersonate-us-from-
  now-on. Keys are now created 0600. Windows ACL hardening remains a gap and is documented at
  the call site rather than left implicit.
- **`verify_token` looped over the length the unauthenticated peer chose to send**, not the
  server's fixed token length, so the number of in-bounds index checks varied with the
  attacker's input — narrow, but a violation of the function's own documented contract. It now
  always runs the expected length.

Also fixed, and verified against the live guest:

- **An unauthenticated denial of service.** The accept loop handshook and served sequentially
  with no timeout anywhere in the crate, so one TCP connection that sent nothing blocked
  `accept()` indefinitely and locked out the operator. Connections now get their own task under
  a single pre-auth deadline, with a bounded number of connections in that phase and one
  authenticated session at a time preserved. Measured on the guest: a silent peer is now closed
  after 20s, and a legitimate TLS handshake completes immediately while three silent peers are
  held — before, it never completed at all.
- A panic in the per-connection path took down the whole process, since sessions were awaited
  directly rather than spawned. Per-connection tasks bound it to one session.

Those tasks use `LocalSet`/`spawn_local` rather than `tokio::spawn`, which is the detail that
would have shipped broken: WinRT and D3D11 interfaces are `!Send`, and the host build never
sees it because the module is `cfg(windows)` — only the Windows cross-compile catches it. The
fix is to keep those tasks on the thread that already owned the COM objects, not to assert
`Send` for them.

Examined and found sound: the pin is checked before any success is returned and the signature
verifiers delegate to the pinned certificate; the pre-auth reassembly bounds hold across every
path through `Reassembler::push`, including channel spreading and completion-and-reuse; and no
allocation happens ahead of length validation anywhere in the decode chain.

**Correction of record:** commit `2d155a5`, whose message describes the H.264 decoder, also
contains the two `oxsec` fixes above. They were staged inadvertently with a broad `git add`
while several changes were in the tree at once. The code in history is correct and complete;
only that commit's message is misleading about what it carries.

### First end-to-end run (2026-07-28)

A Windows application window is now captured in the guest and shown as a **native Linux
window**, live. The whole path ran for real against oxrdp's own dockur guest: WGC capture →
`oxproto` framing → TLS with SPKI pinning and token auth → `oxclient` → `oxdisplay` (winit +
softbuffer). Measured 1115×628 RAW_BGRA at ~21 fps, roughly 470 Mbit/s for a single window —
the concrete case for the P5 H.264 encoder.

Three bugs were found only by running it, none of which the test suite could have caught:

- **WGC pixel format.** The frame pool was created with `B8G8R8A8UIntNormalizedSrgb`.
  `Direct3D11CaptureFramePool` accepts only `B8G8R8A8UIntNormalized` and `R16G16B16A16Float`,
  and rejects anything else with a bare `E_INVALIDARG`.
- **Empty-pool sentinel.** `TryGetNextFrame` reports an empty pool as an `Err` carrying `S_OK`.
  Treating that as a failure made the caller rebuild the capture every tick, so the pool never
  lived long enough to fill and the stream produced zero frames while looking busy.
- **Cancellation safety.** `read_reassembled` keeps its read progress in the future, so a
  `tokio::select!` branch dropped mid-chunk lost the bytes it had consumed and the stream
  resumed mid-payload. Added `ChunkReader`, which keeps that progress in the caller's state;
  `ClientSession` reads through it and buffers writes resumably.

`dev/vm/oxrdp-windows.sh status` was also rewritten: its old probe reported a healthy agent as
"NOT running", because rustls does not answer a truncated handshake with an alert. It now
completes a real TLS handshake and prints the SPKI pin, which was checked against the agent's
own `--print-pin`.

### Direction change (2026-07-02)

oxrdp pivots from "a better RDP **client**" to **replacing RDP itself** with a purpose-built,
low-latency remote-app protocol (RustDesk / Moonlight-style): a Windows guest **agent**
captures individual application windows and streams them to the Linux **client** over a custom
protocol (QUIC, TCP fallback). Rationale: RDP has structural latency limits (TCP head-of-line
blocking by default, bandwidth-optimized buffering, general-purpose overhead) that a
purpose-built protocol can beat. The prior RDP-client work — validated end-to-end through MCS
channel join against a real Windows guest — is retained in git history but **shelved**; its
client shells (TLS, transport, wgpu decode, window mapping, input) and the bounds-checked codec
base carry over. New agent architecture: Rust + `windows-rs`, Windows.Graphics.Capture, runtime
HW/SW encode, QUIC+TCP transport.

- **P0 — `oxproto`.** The new protocol's sans-io wire messages: a `Message` envelope with
  ClientHello / ServerHello / WindowCreated / WindowClosed / FrameData / PointerEvent, built on
  the reused `oxrdp-pdu` codec. 7 tests.
- **P1 setup — cross-compile pipeline + `oxagent` skeleton.** The Windows guest agent
  cross-compiles from Linux to `x86_64-pc-windows-gnu` (mingw-w64): a `oxagent.exe` that links
  `windows-rs` 0.58 with Windows.Graphics.Capture + Media Foundation + Win32 window
  enumeration. The Windows deps are `cfg(windows)`-gated, so the workspace still builds
  `oxagent` as a stub on Linux and CI stays green — the agent is developed and built entirely
  from the Linux host, no in-guest toolchain needed.
- **Gap audit + hardening.** A multi-agent audit (56 adversarially verified findings,
  `docs/design/AUDIT-2026-07.md`) drove: CI that cross-compiles and lints the Windows agent,
  a blocking `cargo audit` plus `cargo deny` for licenses/bans/sources, a pinned toolchain, and
  corrections to claims the docs asserted but had not validated (the VA-API → wgpu DMA-BUF
  import is now marked unvalidated).
- **P1c — WGC per-window capture.** `oxagent` captures a single window to BGRA through a D3D11
  device, a free-threaded frame pool and a reused staging texture, with row-pitch-aware readback
  and frame-pool recreation on resize. Window enumeration now filters cloaked / tool / child /
  shell windows and reports DWM extended frame bounds, and the process is per-monitor DPI aware.
- **oxproto v1 — the protocol redesign.** Specified in `docs/design/OXPROTO.md` and implemented:
  an 8-byte chunk envelope with fragmentation and per-channel reassembly (so a keyframe cannot
  head-of-line-block input or control), authoritative lengths, per-type size limits enforced
  before allocation, an authenticated handshake with version range and feature negotiation, and
  the message set the first design lacked — keyboard/text/modifier input, bidirectional window
  control, cursor streaming, frame acknowledgement and quality hints, per-stage timestamps for
  latency measurement, display layout with fractional scaling, app identity and icons, errors,
  close and ping/pong. Unknown message types are skipped rather than fatal.
- **P2a — client session.** `oxclient` performs the handshake, negotiates features, answers
  ping/pong transparently, and yields a `ClientEvent` stream.
- **Robustness.** `oxproto` gains deterministic smoke-fuzz tests (arbitrary bodies and chunk
  headers never panic; truncation always errors; a declared length cannot make the receiver
  allocate) and cargo-fuzz targets under `fuzz/`. `SECURITY.md` is rewritten for the inverted
  post-pivot threat model — the agent is now a server that shares screen content and injects
  input — and `docs/design/agent-runtime.md` settles the guest session and deployment model.
  122 tests.
- **P1 — window enumeration + async transport.** `oxagent` enumerates visible top-level
  windows (`EnumWindows` → handle / title / geometry), cross-compile-validated to windows-gnu.
  `oxtransport` frames oxproto messages over any tokio stream (`read_message_bytes` /
  `write_message`, 64 MiB guard). `oxproto` re-exports the `decode` / `encode_vec` codec entry
  points. 90 tests.
- **`oxsec` — TLS for the agent link.** A self-signed agent identity generated on first run and
  persisted to disk, an SPKI-pin `ServerCertVerifier` the client uses in place of hostname
  verification (the pin authenticates the peer, not its name), and a constant-time token
  comparison for the handshake. Deliberately not the old `oxrdp-crypto::TofuVerifier`, which
  accepts any certificate and has no place authenticating a server that shares screen content
  and injects input. 7 tests.
- **P1d — agent session driver.** `oxagent` gains a key/value config loader (a wildcard bind
  address is refused outright, not merely defaulted away from), an auth-gated handshake that
  admits exactly one message before authentication, a per-window frame-pacing budget that drops
  the oldest unacknowledged frame instead of queueing behind it — queueing turns a bandwidth dip
  into unbounded latency, the failure this project exists to avoid — a window registry whose
  protocol ids are never reused within a session (the OS recycles native handles; a recycled id
  would blit new pixels into the wrong native window), and `serve.rs`, the driver that ties them
  together: handshake, window-lifecycle diffing, pacing and ack handling. The platform sits
  behind a `WindowSource` trait, so all of this is unit-tested on the Linux build host; only the
  trait implementation is Windows-only. A review pass hardened the newly landed code further:
  reserved envelope flag bits are now ignored rather than rejected, for forward compatibility,
  and reassembly state — allocated before authentication — is now capped at 64 pending channels
  and 64 MiB total, closing a pre-auth memory-amplification path. 33 tests.
- **Client session, window model and CLI.** `oxclient` gains a `WindowModel` that turns the raw
  `ClientEvent` stream into an ordered list of instructions a display backend executes — create
  this native window, retitle it, restack it — instead of every backend diffing protocol
  messages itself; it deliberately does not retain frame pixels, since frames are large and
  arrive at video rate. A new `oxclient` binary is a bring-up CLI: it connects to the agent over
  pinned TLS, performs the handshake, and prints the event stream while acking frames so the
  agent's pacing budget can advance. The token is only ever read from a file — `--token` on the
  command line is refused, because argv is world-readable. 179 tests.
- **Client display/render architecture decided.** `docs/design/client-display.md` settles the
  Linux client's windowing and presentation stack: `winit` plus an `x11rb` property sidecar owns
  native windows permanently, a CPU presenter on `softbuffer` blits `FrameData(RAW_BGRA)` for
  first pixels (P2b) — no `wgpu`, no GPU code at all — and a `wgpu` presenter in a new `oxrender`
  crate arrives only at the H.264 milestone (P5). Supersedes the `DisplayBackend` sketch in
  `docs/ARCHITECTURE.md` §3 and the "FrameData → wgpu texture" phrasing `docs/HANDOFF.md`
  previously carried. `oxrdp-display`, `oxrdp-render` and `oxrdp-input` are marked for deletion,
  not filled in.

### Highlights (RDP-client era — shelved)

**Project bootstrap.** oxrdp is split out as the standalone, from-scratch Rust RDP engine
behind winpodx, with the v0 goal of drop-in equivalence with winpodx's FreeRDP path.

- Locked the architecture: sans-io pure protocol core + pluggable IO / display / render /
  input shells; X11 + Wayland behind one `DisplayBackend` trait.
- Locked the rendering path: `wgpu` GPU from the start, VA-API hardware H.264 decode with
  an `openh264` software fallback (DMA-BUF zero-copy into `wgpu`).
- Locked the scope: staged protocol surface; v0 targets parity with the exact FreeRDP
  capability set winpodx uses, with NLA/CredSSP deferred (winpodx uses `/sec:tls`).
- Established project structure, MIT license, and bilingual (en/ko) documentation.

### Added
- `README.md` and `docs/ARCHITECTURE.md` — project identity, locked decisions, the
  FreeRDP→oxrdp parity matrix, the crate workspace layout, and the M0–M5 roadmap.
- Community health files (CODE_OF_CONDUCT, CONTRIBUTING, SECURITY, THIRD_PARTY_LICENSES),
  GitHub issue/PR templates, and a Rust CI workflow.
- Cargo workspace scaffold — 12 crates (`oxrdp-pdu`, `oxrdp-core`, `oxrdp-graphics`,
  `oxrdp-channels`, `oxrdp-rail`, `oxrdp-crypto`, `oxrdp-io`, `oxrdp-display`,
  `oxrdp-render`, `oxrdp-input`, the `oxrdp` facade, and the `oxrdp-cli` binary) as
  buildable skeletons; pure core crates `#![forbid(unsafe_code)]`. `cargo build/test/
  clippy/fmt` all green.
- **M0 — `oxrdp-pdu` codec foundation.** Hand-written `Decode`/`Encode` traits over
  bounds-checked `ReadCursor`/`WriteCursor` that never panic on malformed/truncated server
  input, with typed `DecodeError`/`EncodeError`. First framing PDUs: `TpktHeader` (RFC 1006)
  and `X224DataHeader`. Zero external dependencies. 9 unit tests.
- **M0 — connection-setup PDUs.** RDP security negotiation (`NegotiationRequest` /
  `NegotiationResponse` / `NegotiationFailure`, MS-RDPBCGR 2.2.1.1.1 / 2.2.1.2.x) and the
  X.224 Connection Request / Confirm TPDUs (`ConnectionRequest` / `ConnectionConfirm`)
  carrying the negotiation and the `mstshash` routing cookie. 19 unit tests total.
- **M0 — MCS domain PDUs.** PER-encoded `ErectDomainRequest`, `AttachUserRequest` /
  `AttachUserConfirm`, `ChannelJoinRequest` / `ChannelJoinConfirm` (T.125 / MS-RDPBCGR
  2.2.1.3–2.2.1.8), including the MCS user-channel base-1001 initiator offset. 25 unit
  tests total.
- **M0 — MCS Send Data wrapper + sans-io connection state machine.** `SendDataRequest` /
  `SendDataIndication` (the MCS envelope every post-join PDU rides in, with PER length
  encoding) in `oxrdp-pdu`. And `oxrdp-core`'s `ClientConnector` — a `step()`-driven,
  IO-free state machine that runs the X.224 negotiation phase: it emits the TPKT-wrapped
  Connection Request, parses the Connection Confirm, and signals the TLS upgrade plus the
  selected protocol. 33 tests across the two crates.
- **M0 — Connect-Initial foundations.** BER (ASN.1) definite-length / boolean / tag-length
  primitives (`ber`), and the GCC client user-data blocks `ClientCoreData` (CS_CORE),
  `ClientSecurityData` (CS_SECURITY), and `ClientNetworkData` (CS_NET) (MS-RDPBCGR
  2.2.1.3.x). These assemble into the MCS Connect-Initial PDU in a later step. 41 tests
  across `oxrdp-pdu` + `oxrdp-core`.
- **M0 — MCS DomainParameters + server GCC blocks.** The BER `DomainParameters` SEQUENCE
  (target / minimum / maximum sets, with minimal unsigned-integer encoding incl. the
  positive sign byte) and the server-side GCC user-data blocks `ServerCoreData` (SC_CORE)
  and `ServerNetworkData` (SC_NET) carried in the MCS Connect-Response. 50 tests across
  `oxrdp-pdu` + `oxrdp-core`.
- **M0 — MCS Connect-Initial / Connect-Response (Basic Settings Exchange).**
  `ConnectInitial::to_bytes()` builds the BER MCS Connect-Initial wrapping a GCC Conference
  Create Request — the T.124 object identifier, the `Duca` H.221 client key, and the
  target/minimum/maximum `DomainParameters` — around the concatenated client data blocks
  (MS-RDPBCGR 2.2.1.3). `ConnectResponse::from_bytes()` parses the server's Connect-Response
  and extracts the server core/network data (the MCS channel IDs) past the `McDn` server
  key. 56 tests across `oxrdp-pdu` + `oxrdp-core`.
- **M0 — full connection-sequence state machine.** `oxrdp-core`'s `ClientConnector` now
  drives the entire RDP connection sequence as a sans-io `step()` machine: X.224 negotiation
  → TLS-upgrade signal → MCS Connect-Initial → Connect-Response (extracting the server
  channel IDs) → Erect Domain + Attach User → the Channel Join loop → `Connected`. Adds
  `oxrdp-pdu::frame` (TPKT + X.224 data wrap/unwrap). A full simulated-handshake test drives
  the connector end to end. 57 tests.
- **M0 — TLS config + async framing (the impure shells begin).** `oxrdp-crypto` provides a
  rustls `ClientConfig` with a trust-on-first-use certificate verifier (`TofuVerifier`,
  FreeRDP `/cert:tofu` posture) for the post-negotiation TLS upgrade — confidentiality
  without MITM protection; pinning is a planned hardening. `oxrdp-io` gains an async TPKT
  frame codec (`read_frame` / `write_frame`) over a tokio stream. First external
  dependencies: `rustls` (ring provider) and `tokio`. 62 tests.
- **M0 — connection driver + runnable `oxrdp` binary.** `oxrdp-io::connect()` assembles the
  transport end to end: TCP → X.224 negotiation → TLS upgrade (`tokio-rustls`) → MCS
  Connect-Initial through channel join, driving the sans-io `ClientConnector` and returning a
  `Session` (the TLS stream + negotiated channel IDs). The `oxrdp` CLI is now runnable —
  `oxrdp <host[:port]> [username]` performs the handshake and reports the negotiated channels.
  The connect seam is validated against a live server; post-connection phases
  (security/licensing/capabilities, graphics, RAIL) are not implemented yet.
- **M0 — Client Info PDU + security header.** `oxrdp-pdu::client_info` builds the RDP Client
  Info PDU (TS_INFO_PACKET, MS-RDPBCGR 2.2.1.11.1.1): logon flags, domain / username /
  password / alternate-shell / working-dir as UTF-16LE, and the extended info (client
  address, 172-byte time zone, session id, performance flags) — the credentials sent after
  channel join. `security::SecurityHeader` is the Basic Security Header (`SEC_INFO_PKT` /
  `SEC_LICENSE_PKT` flags) that prefixes these MCS payloads. 67 tests.
- **M0 — share framing + licensing.** `oxrdp-pdu::share` adds the `ShareControlHeader` and
  `ShareDataHeader` (TS_SHARECONTROLHEADER / TS_SHAREDATAHEADER) that frame the capability
  exchange and data PDUs. `oxrdp-pdu::license` parses the licensing PDU enough to detect the
  common "valid client — proceed without a license" path (ERROR_ALERT / STATUS_VALID_CLIENT).
  71 tests.
- **M0 — capability exchange.** `oxrdp-pdu::caps` adds the General / Bitmap / Input capability
  sets and a `default_client_capabilities` bundle. `oxrdp-pdu::active` parses the server's
  Demand Active PDU (for the shareId) and builds the client's Confirm Active PDU carrying its
  capability sets. (An incremental capability set — more sets will be added for full Windows
  interop.) 78 tests.
- **M0 — finalization PDUs.** `oxrdp-pdu::finalize` adds the connection-finalization
  data-PDU bodies: Client Synchronize, Control (cooperate / request-control), and Font List.
  This completes the connection-sequence PDU set; wiring them into the connector's
  post-connection sequence (Client Info → licensing → capability exchange → finalization)
  is next. 81 tests.
- **M0 — first live handshake against real Windows. ✅** Validated `oxrdp-cli` against a
  running Windows RDP server: the full connection sequence — X.224 negotiation → TLS → MCS
  Connect-Initial / Connect-Response → Erect Domain → Attach User → channel-join loop —
  completes and the client reaches the negotiated MCS channels. This proves the BER / GCC /
  MCS / DomainParameters byte encoding is correct against real Windows. Fix surfaced by the
  test: CS_CORE now carries the **extended fields** (`highColorDepth` / `supportedColorDepths`
  / `earlyCapabilityFlags`, a 216-byte block) that modern Windows requires — a minimal
  8bpp-only core was silently dropped. Connect-driver phase/hex logging is gated behind
  `OXRDP_DEBUG`.

[Unreleased]: https://github.com/kernalix7/oxrdp/commits/main
