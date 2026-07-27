# oxproto — wire specification (v1)

The protocol between the Linux **client** and the Windows guest **agent**. It streams
*individual application windows*, not a desktop, and is designed for low latency first.

This document is authoritative: `crates/oxproto` implements it, and both `oxagent` and
`oxclient` are written against it. Where code and this document disagree, the document is the
bug report.

Status: **v1 design.** Sections marked *(planned)* are specified but not yet implemented.

---

## 1. Design rules

1. **Nothing may block a frame behind a bigger frame.** Messages are fragmented and carried on
   logical channels so a large keyframe cannot delay input or control. Failing this makes the
   protocol *worse* than what it replaces.
2. **Every latency-relevant event is timestamped on one clock.** A protocol whose whole
   justification is latency must be measurable end to end.
3. **The sender must know how far behind the receiver is.** Frames are acknowledged; the agent
   keeps a bounded number in flight and drops stale ones rather than queueing.
4. **A receiver never trusts a length.** Every message type has a maximum payload size,
   enforced before allocating.
5. **No unauthenticated peer reaches any message handler.** The agent captures screen content
   and injects input; that is only safe behind transport encryption plus authentication.
6. **Unknown message types are skipped, not fatal.** The envelope carries a length so a
   receiver can advance past anything it does not understand — this is what makes the protocol
   extensible without a version break.

## 2. Transport

TCP with TLS 1.3 today; QUIC *(planned)* later — QUIC's independent streams map onto the
channel concept below and remove the remaining transport-level head-of-line blocking.

**Encryption is mandatory.** The agent presents a self-signed certificate generated at first
run. The client pins the certificate's SPKI SHA-256, provisioned out of band by whatever
launches both ends (winpodx controls the guest, so it provisions the pin and the token
together). Trust-on-first-use without pinning is not acceptable for a peer that can inject
input.

## 3. Framing

Every message is one or more **chunks**. A chunk is an 8-byte header plus a payload:

```
 0        1        2        3        4        5        6        7
+--------+--------+--------+--------+--------+--------+--------+--------+
|  type  | flags  |     channel     |                length             |
|   u8   |   u8   |      u16 LE     |               u32 LE              |
+--------+--------+--------+--------+--------+--------+--------+--------+
|                          payload (`length` bytes)                     |
```

- `type` — message type (§5). Carried on every chunk of a fragmented message.
- `flags` — bit 0 `FRAG_MORE` (0x01): more chunks follow for this message. Bits 1–7 reserved,
  **must** be zero and are ignored on receipt.
- `channel` — logical stream (§4). Fragment reassembly is per channel.
- `length` — payload bytes **in this chunk**. Authoritative: the receiver reads exactly this
  many bytes and decodes the body from that slice, so a malformed body can never read into the
  next message.

**Chunk payloads are capped at 32 KiB** (`MAX_CHUNK_PAYLOAD`). Larger messages are fragmented.
The cap is a latency bound: it is the longest a higher-priority channel can be delayed by a
lower-priority one on a single connection.

A receiver keeps at most one partially reassembled message per channel; a chunk that arrives
for a channel with a pending message of a *different* type is a protocol error.

## 4. Channels and priority

| Channel | Contents | Priority |
|---|---|---|
| 0 | control — handshake, ping/pong, errors, quality hints, display layout | highest |
| 1 | input — pointer, keyboard, text (client → agent) | highest |
| 2 | cursor — cursor shape/position/visibility (agent → client) | high |
| 3 | window lifecycle — opened/closed/geometry/title/state/z-order/icon | high |
| ≥16 | video — one channel per window, assigned by the agent in `WindowOpened` | normal |

Senders must schedule by priority: when chunks are queued on several channels, a
higher-priority channel goes first. Video channels are served round-robin among themselves.

## 5. Message types

`C→A` client to agent, `A→C` agent to client.

| Code | Name | Dir | Channel |
|---|---|---|---|
| 0x01 | `ClientHello` | C→A | 0 |
| 0x02 | `ServerHello` | A→C | 0 |
| 0x03 | `Error` | both | 0 |
| 0x04 | `Close` | both | 0 |
| 0x05 | `Ping` | both | 0 |
| 0x06 | `Pong` | both | 0 |
| 0x07 | `QualityHint` | C→A | 0 |
| 0x08 | `DisplayLayout` | C→A | 0 |
| 0x10 | `WindowOpened` | A→C | 3 |
| 0x11 | `WindowClosed` | A→C | 3 |
| 0x12 | `WindowGeometry` | A→C | 3 |
| 0x13 | `WindowTitle` | A→C | 3 |
| 0x14 | `WindowState` | A→C | 3 |
| 0x15 | `WindowZOrder` | A→C | 3 |
| 0x16 | `WindowIcon` | A→C | 3 |
| 0x20 | `FrameData` | A→C | ≥16 |
| 0x21 | `FrameAck` | C→A | 0 |
| 0x30 | `PointerEvent` | C→A | 1 |
| 0x31 | `KeyEvent` | C→A | 1 |
| 0x32 | `TextInput` | C→A | 1 |
| 0x33 | `ModifierSync` | C→A | 1 |
| 0x38 | `WindowControl` | C→A | 1 |
| 0x40 | `CursorShape` | A→C | 2 |
| 0x41 | `CursorPosition` | A→C | 2 |
| 0x42 | `CursorVisibility` | A→C | 2 |

Codes are a permanent registry: never reuse a retired code. `0x80`–`0xFF` are reserved for
private/experimental extensions and must never appear in a release.

## 6. Conventions

- All integers are **little-endian**. `bool` is a `u8`, 0 or 1 (any non-zero decodes as true).
- Strings are `len: u16` followed by `len` bytes of **UTF-8** (no NUL terminator). Invalid
  UTF-8 is a decode error.
- Coordinates are **i32 screen coordinates in the guest's virtual desktop space**, using the
  DWM *extended frame bounds* of a window — the visible frame, matching exactly what capture
  produces. Sizes are `u16` pixels.
- **Clock**: `u64` microseconds on the agent's monotonic clock, zeroed at `ServerHello`. The
  client estimates the offset with `Ping`/`Pong` and never needs wall-clock sync.
- `window_id: u32` is assigned by the agent, **monotonically increasing and never reused**
  within a session (native handles are recycled by the OS; ids must not be).

## 7. Handshake

```
client                                   agent
  |------------- ClientHello ------------->|   (channel 0)
  |<------------ ServerHello --------------|   or Error{AuthFailed} + Close
  |                                        |
  |<----------- WindowOpened … ------------|   agent starts streaming
```

**`ClientHello`** — the only message the agent processes before authentication.

| Field | Type | Notes |
|---|---|---|
| `version_min`, `version_max` | u16, u16 | protocol range the client supports |
| `features` | u64 | capability bitmask (§8) |
| `auth_token` | string | shared secret provisioned out of band; constant-time compared |
| `client_name` | string | for logs |
| `codecs` | `u8` count + that many `u8` | codec ids in descending preference (§9) |
| `display` | `DisplayLayout` body (§10) | the client's outputs and scale |

**`ServerHello`**

| Field | Type | Notes |
|---|---|---|
| `version` | u16 | chosen version, within the client's range |
| `features` | u64 | features the agent will actually use |
| `session_id` | u64 | opaque; identifies this session in logs |
| `codec` | u8 | chosen codec |

If authentication fails the agent replies `Error { code = AUTH_FAILED }`, sends `Close`, and
disconnects — without allocating per-session state. If no version overlaps, `Error { code =
VERSION_MISMATCH }`.

## 8. Feature negotiation

`features` is a bitmask so capabilities evolve independently of the version number.

| Bit | Feature |
|---|---|
| 0 | `CURSOR_STREAM` — cursor is sent separately, never composited into frames |
| 1 | `FRAME_ACK` — receiver sends `FrameAck`; sender applies an in-flight budget |
| 2 | `DAMAGE_RECTS` — `FrameData` may carry damage rectangles *(planned)* |
| 3 | `WINDOW_CONTROL` — client may close/move/resize/activate windows |
| 4 | `TEXT_INPUT` — Unicode text path in addition to scancodes |
| 5 | `ICONS` — window/app icons are sent |
| 6 | `AUDIO` — audio stream *(planned)* |
| 7 | `CLIPBOARD` — clipboard exchange *(planned)* |

A feature is active only if **both** sides set the bit.

## 9. Codecs

Codec ids are a registry; `0` is invalid (so an all-zero field cannot look like a valid codec).

| Id | Codec |
|---|---|
| 1 | `RAW_BGRA` — uncompressed BGRA8, top-down, tightly packed. Bring-up only. |
| 2 | `H264` — Annex-B H.264 *(planned)* |
| 3 | `H265` *(planned)* |
| 4 | `AV1` *(planned)* |

`RAW_BGRA` is bring-up only and must be treated as such: 1920×1080×4 at 60 fps is ~4 Gbit/s.
The bring-up milestone deliberately targets **800×600 at 30 fps (~460 Mbit/s)**, which a
loopback or LAN link can carry; anything larger requires a real codec.

## 10. Display, DPI and scaling

`DisplayLayout` (C→A, also embedded in `ClientHello`) describes the client's outputs:

| Field | Type |
|---|---|
| `count` | u8 |
| per output: `id` u8, `x` i32, `y` i32, `width` u16, `height` u16, `scale_num` u16, `scale_den` u16, `refresh_mhz` u32 |

Scale is a rational (`scale_num`/`scale_den`, e.g. 3/2 for 150%) so fractional scaling is exact.
The agent is per-monitor-DPI-aware and reports window geometry in physical pixels; the client
maps that onto its own outputs. Resending `DisplayLayout` at any time replaces the previous
layout.

## 11. Window lifecycle

`WindowOpened` carries everything needed to create a native window that looks like the app:

| Field | Type | Notes |
|---|---|---|
| `window_id` | u32 | never reused |
| `video_channel` | u16 | channel this window's frames arrive on (≥16) |
| `pid` | u32 | owning process |
| `app_id` | string | executable base name (e.g. `notepad.exe`) — becomes `WM_CLASS` |
| `title` | string | |
| `x`, `y` | i32, i32 | extended frame bounds |
| `width`, `height` | u16, u16 | matches the captured frame size exactly |
| `dpi` | u16 | the window's DPI on the guest |
| `flags` | u32 | bit0 resizable, bit1 has_frame, bit2 topmost, bit3 minimized, bit4 maximized |
| `owner_id` | u32 | owning window id, or 0 — dialogs map to transient-for on the client |

`app_id` and `WindowIcon` exist because native-feeling windows — correct `WM_CLASS`, icon,
taskbar grouping — are the entire point of the project; without them the client shows generic
boxes.

Subsequent changes arrive as `WindowGeometry`, `WindowTitle`, `WindowState`, `WindowZOrder`
(`{ window_id, above_window_id }`, 0 = bottom), `WindowIcon`, and finally `WindowClosed`.

## 12. Video and flow control

**`FrameData`**

| Field | Type | Notes |
|---|---|---|
| `window_id` | u32 | |
| `frame_id` | u64 | monotonic per window |
| `codec` | u8 | |
| `flags` | u8 | bit0 keyframe |
| `width`, `height` | u16, u16 | |
| `captured_us` | u64 | agent clock, when capture completed |
| `encoded_us` | u64 | agent clock, when encoding completed |
| `data` | `u32` length + bytes | codec bitstream (fragmented across chunks as needed) |

**`FrameAck`** (C→A) — `{ window_id, frame_id, decoded_us, presented_us }`, both on the client's
clock; the agent only needs the difference between successive acks and its own send time.

**Flow control.** The agent keeps at most `N` unacknowledged frames per window (default 2). At
the limit it does not queue: it **drops** the oldest undelivered frame and encodes the newest
content instead. Without this, a bandwidth dip silently converts into unbounded latency — the
exact failure that makes naive streamers feel worse than RDP.

**`QualityHint`** (C→A) — `{ window_id (0 = all), target_fps u16, max_bitrate_kbps u32, flags u8
(bit0 prefer_latency_over_quality) }`.

## 13. Input

- **`PointerEvent`** — `{ window_id, x i32, y i32 (window-relative), buttons u8 bitmask,
  wheel_x i16, wheel_y i16 (units of 1/120 of a notch), timestamp u64 (client clock) }`.
- **`KeyEvent`** — `{ scancode u16 (PS/2 set 1), flags u8 (bit0 pressed, bit1 extended),
  timestamp u64 }`. Scancodes, not keysyms: the guest applies its own layout, and the client
  translates from its xkb keymap. This is the only way modifiers and games behave correctly.
- **`TextInput`** — `{ text string }` for IME/Unicode input that has no scancode (Hangul, CJK,
  emoji). Requires `TEXT_INPUT`.
- **`ModifierSync`** — `{ modifiers u16 (shift/ctrl/alt/meta), locks u8 (caps/num/scroll) }`,
  sent on every focus change and periodically. Without it a modifier released while the client
  window was unfocused leaves the guest with a stuck key.
- **`WindowControl`** — `{ window_id, action u8, x i32, y i32, width u16, height u16 }` with
  actions: 1 close, 2 activate, 3 minimize, 4 maximize, 5 restore, 6 move, 7 resize. This is
  what makes the native window a real window rather than a picture of one: closing the Linux
  window must close the Windows app.

## 14. Cursor

Sending the cursor separately (`CURSOR_STREAM`) is a latency decision: composited into the
frame, pointer feedback is pinned to frame latency and every mouse move costs a re-encode.

- **`CursorShape`** — `{ cursor_id u32, width u16, height u16, hotspot_x u16, hotspot_y u16,
  format u8 (1 = BGRA premultiplied), data u32-len + bytes }`.
- **`CursorPosition`** — `{ window_id, x i32, y i32 }` (window-relative).
- **`CursorVisibility`** — `{ visible bool }`.

Shapes are cached client-side by `cursor_id`; a repeated cursor costs 4 bytes, not a bitmap.

## 15. Errors, liveness and shutdown

**`Error`** — `{ code u16, message string }`. Codes: 1 `PROTOCOL`, 2 `AUTH_FAILED`,
3 `VERSION_MISMATCH`, 4 `UNSUPPORTED_CODEC`, 5 `WINDOW_GONE`, 6 `CAPTURE_FAILED`,
7 `INTERNAL`, 8 `TOO_LARGE`.

**`Close`** — `{ reason u16 }`: 0 normal, 1 going away, 2 idle timeout, 3 error.

**`Ping`** — `{ seq u32, sent_us u64 }`; **`Pong`** — `{ seq u32, sent_us u64 (echoed),
agent_us u64 }`. Sent every second by both sides; three missed responses declares the peer
dead. This also yields RTT and the clock offset needed to interpret every other timestamp.

## 16. Size limits

Enforced *before* allocating, from the chunk header alone. A 5-byte header must never be able
to make a receiver allocate megabytes.

| Type | Max payload |
|---|---|
| handshake, control, input, cursor position/visibility | 4 KiB |
| `WindowOpened`, `WindowTitle`, `DisplayLayout`, `Error` | 8 KiB |
| `CursorShape` | 256 KiB |
| `WindowIcon` | 1 MiB |
| `FrameData` (reassembled) | 32 MiB |

Exceeding a limit is `Error { TOO_LARGE }` followed by disconnect. Reassembly buffers are
bounded by the same table, and a receiver must not pre-allocate the declared size — it grows
the buffer as chunks actually arrive.

## 17. Versioning

`PROTOCOL_VERSION` is the current version; `MIN_SUPPORTED_VERSION` the oldest accepted. The
client sends a range, the agent picks. Additive changes (new message types, new feature bits,
appended fields) do not bump the version: unknown types are skipped via the envelope length,
and unknown trailing bytes within a known message are ignored. Only an incompatible change to
an existing field bumps `PROTOCOL_VERSION`.
