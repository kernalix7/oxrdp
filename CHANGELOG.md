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

### The transport tail is connection warm-up, not steady state (2026-07-30)

Localising the tail in time answered a question the aggregate percentiles could not. 172 frames,
one-second buckets, one window:

```
  t/s  frames  out   e->a p50   e->a max   c->e p50   total p50
    0       9     7      80323      84735       9119      100665
    1      21     1       9778      80329       7290       24331
    2      19     0       1018       9669       6888       15475
    3      21     0       1123      41454       6169       14716
   ...
    9      18     0        991      41528       8985       16662
8 outliers, in 2 of 11 buckets; the heaviest holds 7
slowest frames: w1/f1 84735, w1/f4 81323, w1/f5 80473, w1/f8 80470, w1/f10 80329, ...
```

Every outlier is in the first second and a half, and the named frames are the session's **first
ten**. After that, `encode->arrival` sits at about **1 ms** with no outlier in nine seconds — and
`capture->encode` is unremarkable through the burst, so it is the transport stage specifically,
not the pipeline stalling together.

So the p95 and p99 figures quoted for `encode->arrival` all week were connection warm-up sampled
by short runs, not a steady-state property. That is why six explanations inside the code all failed
to account for it: none of them was about the first ten frames of a TCP connection.

Two phenomena, not one, and the second is easy to miss: the startup burst at ~80 ms, and a
recurring ~40 ms spike visible in the `max` column of nearly every later bucket. Those later
spikes count as zero outliers only because the threshold — the timeline's own p95 — is itself
inflated by the burst. They are the thing left to explain.

The benchmark had to be fixed twice to get here, and both failures are worth recording because
they produced confident-looking numbers. `timeout /t 1` repaints once a second, so a 60-second run
yielded 61 frames with every frame isolated — no back-to-back condition, and too few samples to
distinguish a clustered tail from a spread one. `ping -w` as a sub-second sleep was worse, since
its real delay depends on how the unreachable address fails. The committed benchmark paces with
PowerShell's `Start-Sleep -Milliseconds`, which means what it says.

### The transport tail is not ours (2026-07-30)

The agent now measures its own half of the one stage nothing had explained. `encode->arrival`
spans from the encoder finishing to the client reading a frame, and that contains the agent's path
to the socket as well as the network. Split agent-side, no wire change, 243 frames:

```
queue_wait_us        p50    76 us   p95   240   max 6,149
socket_write_us      p50   170 us   p95   617   max 2,420
encode_to_write_us   p50   252 us   p95 1,248   max 7,088
```

The whole "the agent still had it" span is a quarter of a millisecond at p50 and 1.2 ms at p95.
The client observes that same stage at p50 955 us and p95 ~9 ms. **Roughly 7.7 ms of the p95
happens after the socket write returned** — in the TCP buffer, the port forward, and the client's
read.

That is the signal predicted in advance when the measurement was built: a small agent-side span
beside a large observed one is what it looks like when the tail lives between the syscall
returning and the bytes actually moving. It closes the question with positive evidence rather than
by elimination, and it agrees with the independent clue already on record — ping/pong round trips,
tiny control messages that no frame-size, flow-control or Nagle mechanism can touch, ranged from
1 ms to 21 ms across runs and once implied ~100 ms.

Six explanations inside this project's own code were eliminated to get here, each by measurement:
keyframe size, flow-control backlog, the client's read pauses, Nagle alone, the agent's send path,
and now the agent's whole pre-wire span. **The remaining latency tail belongs to the environment —
a QEMU port forward over passt, on a WiFi host where the guest cannot have its own L2 address, so
there is no direct path to compare against.** Measuring oxrdp's real transport latency needs a
different host, and until then `encode->arrival` and the end-to-end total are bounded below by our
code and above by the environment.

One caveat kept deliberately: the agent's span and the client's are from adjacent runs rather than
the same one, so the 7.7 ms is a difference between distributions rather than per-frame. Frame-id
correlation is in the logs and would tighten it; the conclusion does not rest on the exact figure,
because the agent's span never exceeds 7 ms even at its maximum while the client's p99 reaches 36.

Also fixed here: the agent's session check asked `WTSGetActiveConsoleSessionId`, which answers who
owns the physical console rather than whether this session has a desktop that can receive input.
On this guest that produced a false alarm — dockur's autologon owns session 1 and an RDP client
takes it over — so it warned that input could not work while input demonstrably worked. It now
queries its own session's connect state and explains the topology instead of alarming about it.

### A rebuilt guest, a controlled benchmark, and the GPU path gated (2026-07-30)

The guest had accumulated twenty-odd windows and stuck dialogs across two days of testing, and
every measurement was being taken against that. Rebuilt from scratch — which also validated the
OEM self-provisioning path for the first time: dockur copied `dev/vm/oem/` into the guest, ran
`install.bat` unattended, and the agent came up on its own with a scheduled task, its identity
generated, listening in the interactive session. That path had been written and marked untestable
against the old guest.

Two things follow from measuring on a clean guest with a fixed-rate benchmark instead of a `cmd`
loop whose output rate followed the guest scheduler.

**The GPU NV12 conversion is 7.5x slower here, and is now gated on the device being real.**

```
capture->encode   CPU path   p50  6,542 us
                  GPU path   p50 49,224 us
END TO END        CPU path   p50 13,195 us
                  GPU path   p50 59,628 us
```

This guest has no GPU passthrough, so `create_d3d_device` falls back to WARP and the "GPU" video
processor is software — slower than the hand-written CPU loop, and now also allocating an NV12
texture per frame. The path is offered only on a hardware device, and which device was chosen is
logged, because until now the code fell back to WARP with nothing to say so. The conversion
itself is correct and will pay on a real GPU; what it lacked was any notion that "GPU" might not
mean a GPU.

**The noise floor collapsed, which makes the numbers usable.** Within one run, `encode->arrival`
p50 now varies 1.2x and the frame rate 1.0x, against 3x a day earlier. A difference larger than
1.2x is now a signal rather than something the environment could have produced on its own — and
the earlier conclusion that four in-code explanations for the transport tail were all wrong holds
up better for it.

One finding from the rebuild worth recording on its own: the agent's session diagnostic fired with
`running in session 1, but the active console session is 3`. That is a false alarm in this
topology — dockur's autologon owns session 1 and an RDP client reconnecting as the same user takes
it over, leaving a fresh console session behind — but the underlying point is real: an agent
started by a logon task is in whichever session logged on, and `WTSGetActiveConsoleSessionId` is
the wrong question to ask when the user is on RDP.

### Sends off the tick loop, and the tail is the VM's port forward (2026-07-30)

`drive()` handled each tick in one task, awaiting socket writes, so a blocked send delayed the
next capture for every window. Writes now go to their own task over a bounded channel through
`ChunkWriter`. Measured on the guest, release both sides:

```
                    before          after
capture->encode     p95 19,623      p95  5,955     p99 43,690 -> 6,935
encode->arrival     p50  6,943      p50  1,522
END TO END          p50 24,856      p50 10,411
```

The stage that was three quarters unexplained a day ago is now tight, and the end-to-end median
halved. The property that had to survive did: a frame reserves its send slot **before** it is
captured and encoded, so no frame is ever encoded and then dropped — which would corrupt every
frame after it until the next keyframe, with no way for a client to ask for one. `ChunkWriter`'s
cancel-safety, added when nothing could yet cancel a write mid-chunk, is load-bearing now.

**What did not move is the answer to the rest.** `encode->arrival` p95 stayed at 44 ms with the
writer on its own thread, and five explanations inside our own code have now been eliminated:
keyframe size, flow-control backlog, the client's read pauses, Nagle alone, and the agent's send
path. The evidence was already in the reports: **ping/pong round trips, which are tiny control
messages with no payload for any of those mechanisms to act on, ranged from 1.0 ms to 21 ms
across runs, and once implied ~100 ms.**

So the tail belongs to the path — loopback through this VM's port forward — not to the protocol.
That also explains why every candidate inside the code died: there was nothing there to find.
Measuring oxrdp's own transport latency needs a path that is not a QEMU port forward, and until
then `encode->arrival` and the end-to-end total should be read as bounded below by our code and
above by the environment.

### The capture-to-encode stage, fully accounted for (2026-07-30)

The agent now times every piece of the largest stage. On the guest, release, one run:

```
tick_to_capture      295 us   (p95 100,916)
pool_acquire           5 us
copy_resource         15 us
map                1,199 us
readback_copy        953 us
convert            1,118 us
process_input        214 us
process_output        14 us
                   ------
sum of medians     3,812 us
```

The client measured `capture->encode` at 3,720 µs p50 in the same run, so the sum of medians
lands within 2.5% of the stage it claims to explain. Nothing is unattributed any more.

Two things follow, and the second is more important than the first.

**The GPU readback, not the colour conversion, is the largest cost.** `map` at 1,199 µs plus
`readback_copy` at 953 µs is 2.15 ms — more than the 1,118 µs conversion. `map` being the bigger
half is the predicted shape: `CopyResource` only queues GPU work, while `Map` on a staging
texture must block until it completes, so on a virtualised GPU this is where the wait across the
boundary appears. That changes what the deferred D3D11 video-processor path is worth: keeping the
frame on the GPU would remove the map, the readback *and* the conversion together — 3.3 ms of a
3.7 ms stage — rather than the conversion alone, which is what a SIMD pass would address.

**`tick_to_capture` has a median of 295 µs and a p95 of 100,916 µs.** A 340x spread on the gap
between the ticker firing and capture starting is not jitter. `drive()` handles the tick in a
single task: `sync_windows` and then `pump_frames`, both of which `await` socket writes. So a
send that blocks stalls the loop, and the next tick's capture is delayed behind it — for every
window, not just the one that was slow.

That makes the unexplained `encode->arrival` tail and these capture stalls candidates for being
**one phenomenon rather than two**: a slow send delays the next capture, which delays the next
send. Four separate explanations for that tail have now been eliminated inside the transport
stage; this is the first evidence that the coupling lives outside it, in the agent's own loop
structure. It is an architecture question — capture, encode and send are one serial decision —
rather than a tuning one.

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
and a software decoder, and no GPU anywhere in the path. **Read that figure as a lower bound:**
a review of the instrument afterwards found the presenter timestamps its work on a private clock
rather than the session's, so every stage built on presentation — decode-to-present, the
client-only span, and the end-to-end total — is biased low by however long elapses between the
session connecting and the display thread starting. The stage-sum test cannot catch it, because
the same biased term appears on both sides of the identity and cancels. The tail is the finding: p95 and p99 are
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

**Correction, same day: the tail was attributed too confidently.** The transport hop was
diagnosed as Nagle — the agent never set `TCP_NODELAY`, unlike the client — and the fix landed.
The within-run evidence for it is real: measured in a single run, small frames were penalised
3.9x relative to large ones before the fix and 1.5x after, which is the direction Nagle removal
predicts and a comparison that controls for run conditions.

But the before-and-after **cannot** be told apart. Four runs of the identical fixed
configuration produced `encode->arrival` medians of 4.1, 7.1, 8.3 and 12.3 ms and p99s of 53,
109, 169 and 247 ms — and the pre-fix run sits comfortably inside that spread. Run-to-run
variance is larger than the effect being measured, so the claim that the fix was observed
working is withdrawn. The change stays because disabling Nagle on a latency-sensitive stream is
correct on its own terms, not because a measurement here proved it.

The lesson is about the experiment rather than the instrument: a report that prints one run's
percentiles invites exactly this mistake, and comparing two builds needs repetition and a stated
threshold for what counts as a difference. The most stable number across every run was
`capture->encode` at 5.7-8 ms, which is the agent's CPU colour conversion — the one stage not
drowning in noise.

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
