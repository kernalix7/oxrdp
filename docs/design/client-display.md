# Client display & render stack

Decision record, 2026-07-28. This settles the display/render architecture for the Linux client:
what presents pixels at the first-pixels milestone (P2b), what presents them at the H.264
milestone (P5), and the boundary between windowing and rendering. It supersedes the
`DisplayBackend` sketch in `docs/ARCHITECTURE.md` §3 (pre-pivot, banner'd Superseded) and the
"FrameData(RAW_BGRA) → wgpu texture" phrasing in `docs/HANDOFF.md` P2b.

Related: [`OXPROTO.md`](OXPROTO.md) §9 (codecs), §11 (window lifecycle), §14 (cursor);
[`AUDIT-2026-07.md`](AUDIT-2026-07.md) findings 5, 6, 26, 28.

---

## 1. The decision

**One display stack, two presenters.** The stack splits at a hard seam:

- **`oxdisplay` (window backend)** — owns native window lifecycle, WM identity (title,
  `WM_CLASS`/app_id, icon, transient-for), input capture, and the platform event loop.
  Built on **winit** (one event loop, N toplevels), with an **x11rb property sidecar** on X11
  for anything winit does not expose. This layer is permanent — it does not change at P5.
- **`Presenter` (render backend)** — puts one frame into one already-existing native window,
  behind a small trait keyed on `raw-window-handle`.
  - **First pixels (P2b): a CPU presenter on softbuffer.** No wgpu, no GPU code at all.
  - **H.264 (P5): a GPU presenter on wgpu** in a new `oxrender` crate, owning VA-API decode
    and the NV12→RGB conversion shader. The upload path (DMA-BUF zero-copy vs CPU copy of the
    decoded surface) is decided by the spike the audit already mandates (§7, S3) — the wgpu
    commitment itself does not depend on the spike's outcome.

The empty `oxrdp-display` / `oxrdp-render` crates are **deleted**, not filled in (§6).

### Why this, actually

1. **This client never composites.** One remote window = one native window = one surface
   exactly the size of the frame. There is no scene graph, no scaling, no blending — the
   display server composites windows, and the cursor rides the hardware cursor plane (§5).
   Until decode output lives on the GPU, the "renderer" is a memcpy, and a memcpy does not
   need a wgpu device.
2. **Every frame is CPU-side BGRA until P5 by protocol definition.** RAW_BGRA (OXPROTO §9) is
   the only codec, arriving in `FrameData.data` in client memory. A wgpu path now is: adapter
   selection, surface/format negotiation, a blit shader, resize/present-mode races, device-loss
   handling — real code with real failure modes, saving nothing.
3. **softbuffer's pixel format is memcpy-compatible with RAW_BGRA.** softbuffer buffers are
   `u32` `0x00RRGGBB` native-endian; on little-endian that is byte order B,G,R,X — exactly
   RAW_BGRA with alpha ignored. Present is a bounded copy, ~55 MB/s at the bring-up target
   (800×600×4×30). First pixels is a *protocol and agent* validation milestone; the display
   must be the most boring component in it.
4. **Building wgpu now would be the "wrong thing twice", not the hedge against it.** Audit
   finding 6: DMA-BUF import has no public wgpu API; it needs `wgpu_hal`, and the required
   Vulkan extensions must be enabled *at device creation*, so the device must be built via hal.
   Any ordinary `request_device` setup written at P2b would be rewritten at P5 anyway. Deferring
   wgpu until the P5 spike means writing its initialization once, informed by the spike.
5. **The cost accounting is lopsided.** CPU presenter: ~200 LOC, kept forever as the
   RAW_BGRA/debug/fallback path (GPU-less machines, driver breakage, `--presenter=cpu`).
   wgpu-now: ~800+ LOC with roughly half rewritten at P5.

So: **the first-pixels stack is not throwaway.** The windowing layer is the P5 layer; the CPU
presenter is demoted to fallback, not deleted; the only component that could be discarded is
the winit *backend itself*, and that risk is bounded by the trait seam and settled by cheap
spikes (§7) before the backend is built out.

### Why winit and not raw x11rb/sctk

The hard requirement is one native toplevel per remote window with correct `WM_CLASS`, title,
icon, transient-for, and per-window input. Assessment, stated with confidence levels:

- **Per-window `WM_CLASS` / app_id: yes, asserted.** winit exposes
  `WindowAttributesExtX11::with_name(general, instance)` (WM_CLASS, per window) and
  `WindowAttributesExtWayland::with_name` (xdg app_id, per window).
- **Title: yes.** `set_title`, per window.
- **Icon: X11 yes** (`set_window_icon` → `_NET_WM_ICON`); **Wayland no** — winit has no
  xdg-toplevel-icon path. On Wayland the reliable icon/taskbar route is app_id matching a
  generated `.desktop` file regardless of toolkit (audit 26), so winit costs nothing extra here.
- **Many toplevels in one event loop: yes, asserted** — multi-window is the standard
  `ApplicationHandler` model, windows keyed by `WindowId`. Dozens of borderless toplevels is
  less exercised in the wild than 2–3, so the S1 spike smoke-tests it, but this is validation,
  not doubt about the design.
- **Transient-for: not exposed by winit — this is the real gap.** On X11 it is fully
  recoverable: a second x11rb connection can set `WM_TRANSIENT_FOR` (and any other property)
  on winit's XID — X11 properties are settable by any client. That sidecar is the escape hatch
  for *everything* winit lacks on X11 (`_NET_WM_WINDOW_TYPE`, motif hints, restacking). On
  Wayland there is no sidecar — protocol objects belong to winit's connection — so
  transient-for on Wayland is unavailable until winit grows `set_parent` or the Wayland backend
  is written on smithay-client-toolkit. **Spike S2 settles which** (§7).
- **Custom cursors: yes** — winit 0.30's `CustomCursor::from_rgba` with hotspot, X11 and
  Wayland. This is load-bearing for §5.
- **Undecorated windows: yes** (`with_decorations(false)`), and that is what we want —
  captured frames use DWM extended frame bounds, so the Windows chrome is *in the pixels*;
  local decorations would double-frame every window. This also sidesteps Wayland CSD entirely.

Raw x11rb/sctk backends would buy total control at the price of hand-writing two windowing
stacks including input, output enumeration, and DPI — weeks of work that delays first pixels
and duplicates what winit does adequately. The trait seam keeps that door open per-platform:
if S1/S2 fail, a native backend replaces the winit one behind the same API and nothing above
the seam changes. **Sequencing: X11 first** (as HANDOFF already says); Wayland users run the
client under XWayland until the Wayland backend milestone, which is when S2's answer is needed.

## 2. The stack, concretely

### First pixels (P2b)

| Layer | Choice |
| --- | --- |
| Event loop / windows | winit 0.30.x — one `EventLoop`, one borderless toplevel per `WindowOpened` |
| WM identity | winit `with_name` (WM_CLASS/app_id) + `set_title` + `set_window_icon` (X11); x11rb sidecar for `WM_TRANSIENT_FOR` from `owner_id` |
| Present | softbuffer 0.4.x — `FrameData.data` (RAW_BGRA) reinterpreted as `&[u32]`, copied row-wise, `buffer.present()` |
| Cursor | winit `CustomCursor` from `CursorShape`, cached by `cursor_id`; never drawn into frames |
| Input | winit events → protocol messages; `PhysicalKey::Code` → PS/2 set-1 scancode via static table (lives in `oxdisplay`) |
| Session ↔ display | tokio task (session) ⇄ main thread (winit) via `EventLoopProxy` (commands in) + mpsc (events out) |
| Alpha | ignored — opaque windows; Win11 rounded corners render square/dark. Accepted bring-up artifact, fixed by the GPU presenter. |

### H.264 (P5)

| Layer | Change |
| --- | --- |
| Event loop / windows / identity / input | **unchanged** |
| Present | `oxrender::GpuPresenter` (wgpu): H.264 → VA-API decode → NV12 upload → convert shader → surface per window. Device built via `wgpu_hal` so the dmabuf-import extensions *can* be enabled if S3 validates zero-copy; otherwise `vaDeriveImage`/`vaMapBuffer` → `queue.write_texture` (correctness-first path, audit 6's own fallback). RAW_BGRA still supported (plain texture upload). |
| Cursor | unchanged — still native, still not composited |
| Alpha | GPU presenter presents premultiplied alpha into transparent surfaces (ARGB visual / Wayland alpha), fixing rounded corners |
| Fallbacks | CPU presenter retained (`--presenter=cpu`, RAW_BGRA only). NVIDIA-without-VA-API decision comes out of S3 (NVDEC vs software decode). |

The presenter is selected at startup, not per-frame. Nothing above the `Presenter` trait knows
which one is running.

## 3. The seam: what `oxdisplay` presents to `oxclient`

`oxclient` stays the protocol-session library (`ClientSession`, `ClientEvent`). `oxdisplay`
consumes a command enum that mirrors the display-relevant `ClientEvent`s and emits a
display-event enum that the session task translates into `oxproto::Message`s. Sketch —
normative for shape, not for names of helper types:

```rust
// ---- oxdisplay public surface ----------------------------------------------

use oxproto::message::{
    CursorPosition, CursorShape, CursorVisibility, FrameData, WindowClosed, WindowGeometry,
    WindowIcon, WindowOpened, WindowState, WindowTitle, WindowZOrder,
};

/// Session thread → display thread. One-to-one with the ClientEvents the
/// display layer consumes; the session task forwards, it does not interpret.
pub enum DisplayCommand {
    OpenWindow(WindowOpened),
    Geometry(WindowGeometry),
    Title(WindowTitle),
    State(WindowState),
    ZOrder(WindowZOrder),
    Icon(WindowIcon),
    CloseWindow(WindowClosed),
    Frame(FrameData),               // undecoded bitstream; decode belongs to the Presenter
    CursorShape(CursorShape),
    CursorPosition(CursorPosition), // ignored in v1, accepted for forward-compat (§5)
    CursorVisibility(CursorVisibility),
    Shutdown,
}

/// Display thread → session thread. The session task translates these into
/// oxproto messages. All coordinates are physical pixels; pointer coordinates
/// are window-relative, per OXPROTO §13.
pub enum DisplayEvent {
    // → PointerEvent
    Pointer { window_id: u32, x: i32, y: i32, buttons: u8, wheel_x: i16, wheel_y: i16 },
    // → KeyEvent (already translated to PS/2 set 1 — the table lives here
    //   because winit key types live here)
    Key { scancode: u16, pressed: bool, extended: bool },
    // → TextInput (only when the TEXT_INPUT feature is active)
    Text { text: String },
    // → ModifierSync (emitted on every focus gain and periodically)
    Modifiers { modifiers: u16, locks: u8 },
    // → WindowControl { action: activate } + a ModifierSync
    Focused { window_id: u32, focused: bool },
    // → WindowControl { action: close }
    CloseRequested { window_id: u32 },
    // → WindowControl { action: resize } — local size always wins (§4)
    ResizeRequested { window_id: u32, width: u16, height: u16 },
    // → WindowControl { action: move } — X11 only; never emitted on Wayland (§4)
    MoveRequested { window_id: u32, x: i32, y: i32 },
    // → WindowControl { action: minimize | restore }
    Minimized { window_id: u32, minimized: bool },
    // → FrameAck { decoded_us, presented_us } (client monotonic clock, µs)
    Presented { window_id: u32, frame_id: u64, decoded_us: u64, presented_us: u64 },
    // Fatal backend failure; the session should Close and exit.
    BackendError { message: String },
}

/// Anything a Presenter can bind a surface to. oxdisplay guarantees the handle
/// outlives the attach..detach interval.
pub trait DisplayWindow: raw_window_handle::HasWindowHandle
    + raw_window_handle::HasDisplayHandle {}

/// Puts pixels into an existing native window. Implementations:
///   - oxdisplay::CpuPresenter (softbuffer; RAW_BGRA only)     — P2b, kept as fallback
///   - oxrender::GpuPresenter  (wgpu; H.264 + RAW_BGRA)        — P5
pub trait Presenter {
    fn attach(&mut self, id: u32, window: &dyn DisplayWindow,
              width: u32, height: u32) -> Result<(), PresentError>;
    /// Native surface size changed (compositor-driven). Until the guest catches
    /// up, present() anchors the frame top-left and clears the remainder (§4).
    fn resize(&mut self, id: u32, width: u32, height: u32) -> Result<(), PresentError>;
    /// Decode + present one frame. Timestamps feed FrameAck. For the CPU
    /// presenter, decoded_us = copy complete, presented_us = present() returned
    /// (honest approximation; refined at P5 with real presentation feedback).
    fn present(&mut self, id: u32, frame: &FrameData) -> Result<PresentTimes, PresentError>;
    /// Re-present the last frame (expose / RedrawRequested). Presenters keep
    /// the last frame per window for this.
    fn refresh(&mut self, id: u32) -> Result<(), PresentError>;
    fn detach(&mut self, id: u32);
}

pub struct PresentTimes { pub decoded_us: u64, pub presented_us: u64 }

/// Enumerate outputs for SessionConfig.display (DisplayLayout) before connecting.
pub fn outputs() -> Vec<oxproto::message::Output>;

/// Runs the platform event loop on the calling thread (must be the main thread)
/// until DisplayCommand::Shutdown or fatal error. `ready` fires once the loop is
/// live, handing the session side its command channel; the closure spawns the
/// tokio runtime + session task.
pub fn run(
    presenter: Box<dyn Presenter>,
    events: tokio::sync::mpsc::UnboundedSender<DisplayEvent>,
    ready: impl FnOnce(CommandSender),
) -> Result<(), DisplayError>;

/// Cloneable, thread-safe; wraps winit's EventLoopProxy so sends wake the loop.
pub struct CommandSender { /* … */ }
impl CommandSender { pub fn send(&self, cmd: DisplayCommand) -> Result<(), Closed>; }
```

Wiring notes (binding, not optional):

- **Threading.** winit owns the main thread; the tokio runtime and `ClientSession` live on a
  spawned thread. `CommandSender` is the only way in; the mpsc is the only way out.
- **Window identity.** Everything is keyed by the protocol `window_id: u32`. The winit
  `WindowId` ↔ `window_id` map is private to `oxdisplay`.
- **Frame routing.** `Frame` for an unknown `window_id` (race against `CloseWindow`) is
  dropped silently. The agent's 2-frame in-flight budget (OXPROTO §12) means the display
  thread never queues meaningfully; commands are processed newest-last with no coalescing.
- **Echo suppression.** Locally-initiated resize/move produce a `WindowControl`, which the
  guest answers with `WindowGeometry`. `oxdisplay` keeps a small pending-op ledger per window
  and swallows geometry commands that match an outstanding local op, breaking the loop.
- **`WindowState`** maps to `set_minimized` / `set_maximized`; the `topmost` flag maps to
  `WindowLevel::AlwaysOnTop`. **`WindowZOrder` is ignored in v1** — winit cannot restack
  siblings, Wayland has no restacking at all, and the X11 sidecar's `ConfigureWindow` restack
  is WM-dependent. Accepted gap; revisit with the popup/menu milestone.
- **`WindowIcon`** → `set_window_icon` on X11; no-op on Wayland. The `.desktop`-file/icon-cache
  subsystem (audit 26) is a separate `oxclient` module and out of scope here; `oxdisplay` just
  surfaces the icon bytes to it.
- The new thin binary that wires `ClientSession` + `oxdisplay::run` is `oxclient-cli`
  (the shelved `oxrdp-cli` stays shelved).

## 4. X11 vs Wayland: geometry policy

The protocol carries absolute guest coordinates (i32, guest virtual-desktop space, OXPROTO §6).
Wayland toplevels can neither set nor even *know* their global position. This is a semantic
split, and the client resolves it with four normative rules:

1. **Size: the local side always wins.** On both backends, a compositor/user resize is
   accepted immediately, sent to the guest as `WindowControl{resize}`, and the guest follows.
   (On tiling compositors the size is dictated; there is no choice.) While frame size and
   surface size disagree, the presenter anchors the frame **top-left and clears the
   remainder** — no scaling in the CPU presenter, ever; the GPU presenter may scale the stale
   frame at P5 but still snaps when the correctly-sized frame arrives. Guest size is
   authoritative only for the *pixel dimensions of the frame*.
2. **Position on X11: the guest wins, and local moves are sent as *displacements*.**
   `WindowOpened`/`WindowGeometry` positions are applied via `set_outer_position`; a local drag
   emits `MoveRequested` → `WindowControl{move}`. What goes on the wire is the guest's own last
   known position plus how far the window just moved — **never an observed local position**.

   > **Corrected 2026-07-28.** This rule previously said the mapping was identity, because
   > winpodx provisions the guest desktop to mirror the client's layout. That premise does not
   > hold for oxrdp's own guest: it is a 1280x800 desktop while the client's X screen extends
   > past x=3200, and taking it literally is what shipped a real bug — connecting the client
   > pushed host coordinates into the guest as if they were guest coordinates, leaving windows
   > moved off the desktop and, in one case, resized to 1x52. Do not restore the identity
   > mapping on the strength of a claim about how the guest was provisioned; oxrdp does not
   > control that, and a client cannot even check it, because there is no agent→client message
   > carrying the guest's desktop bounds.
   >
   > A displacement is the one quantity that means the same thing in both spaces, so it needs
   > no such message. Where the mirroring premise *does* hold, a displacement is the identity
   > mapping anyway — this is a weaker assumption, not a different design.

   Two consequences worth stating, because both were learned the hard way. Sending a
   displacement requires **two** local observations, which is what makes it structurally
   impossible to echo the window manager's own initial placement back to the guest as if the
   user had dragged something — a first report can only become an anchor. And a geometry change
   the *client* caused is not user intent: creating a window, and applying a guest-originated
   move, must each open a settling window during which local reports only update what the
   client believes.

   Two guards belong with this rule rather than in the implementation that happens to hold them
   today. A window the guest reports as non-resizable must never be sent a resize — a
   fixed-size dialog told to become 1x52 is destroyed. And a report of zero width or height is
   never forwarded: some window managers report 0x0 for an unmapped window, which would ask the
   guest to resize to nothing.
3. **Position on Wayland: never applied, never reported.** Guest x/y is stored as data (future
   popup/dialog anchor math) but no positioning is attempted, and `MoveRequested` is never
   emitted — the client cannot observe local moves on Wayland. Consequence, stated honestly:
   **multi-window spatial relationships do not hold on Wayland** — an app that positions a
   palette next to its main window will get compositor-chosen placement. Accepted v1
   limitation. Mitigations arrive later via `xdg_toplevel.set_parent` (dialogs follow their
   parent) and `xdg_popup`/`xdg_positioner` (menus/tooltips), which is Wayland-backend-
   milestone work and feeds the S2 winit-vs-sctk decision.
4. **Input does not depend on any of this.** `PointerEvent` is window-relative by protocol
   design (OXPROTO §13), so positional drift on Wayland is invisible to the guest. This rule
   is why Wayland is viable at all; do not add any absolute-coordinate input path.

## 5. Cursor

Per OXPROTO §14 the cursor is streamed separately, deliberately. The client-side consequence:

- **The cursor is composited by the display server, never by the presenter.** `CursorShape`
  becomes a winit `CustomCursor` (convert BGRA *premultiplied* → the RGBA straight-alpha winit
  expects), cached by `cursor_id` — a repeat shape costs a map lookup. `CursorVisibility`
  maps to `set_cursor_visible` on the hovered window.
- The payoff: local pointer motion costs **zero** remote round trips — the native cursor
  tracks the local pointer at compositor speed; only shape *changes* pay RTT. This is the
  resolution of audit finding 5 and must not be regressed by any future "draw the cursor in
  the frame" shortcut. The agent keeps WGC `IsCursorCaptureEnabled(false)`.
- **`CursorPosition` is ignored in v1.** It matters only when the *guest* moves the pointer
  (games, `SetCursorPos`). Warping the user's pointer is impossible on Wayland and hostile on
  X11. This belongs to a future pointer-lock/relative-input milestone; the command is accepted
  and dropped so the protocol needs no change.

## 6. Crate disposition

- **`oxrdp-display` — delete.** 6-line pre-pivot skeleton, zero code, RAIL-era doc comment
  that actively misleads. Git keeps it.
- **`oxrdp-render` — delete.** Same. Its doc comment ("wgpu compositing") encodes exactly the
  pre-pivot assumption this document retires.
- **`oxrdp-input` — delete alongside.** Input capture is inseparable from the event loop, so
  its role is absorbed by `oxdisplay` (the scancode table lives there). No third crate.
- **New: `oxdisplay`** (new-direction naming, alongside `oxproto`/`oxtransport`/`oxclient`/
  `oxagent`): the §3 surface, the winit backend, the x11rb sidecar, the CPU presenter, the
  scancode table. Created at P2b.
- **New: `oxrender`** — created **at P5, not before**: `GpuPresenter`, wgpu device (hal-built),
  VA-API decode, NV12 shader. Depends on `oxdisplay` for the `Presenter` trait. Keeping GPU
  and libva deps out of `oxdisplay` is the point of the second crate.
- **`oxclient`** stays the session library; gains nothing display-related. `oxclient-cli` is
  the new thin binary.

The shelved, *non-empty* `oxrdp-*` crates are untouched — the deferred repo rename (README
"Name") is a separate decision and nothing here blocks on it.

## 7. Spikes and open questions

Each spike names what settles it. The architecture above does not change with spike outcomes —
only the named slot inside it does.

- **S1 — winit-X11 identity + multi-toplevel smoke test** *(before building out `oxdisplay`;
  ~1–2 days).* Create 8 borderless toplevels with distinct `with_name`, titles, icons; verify
  alt-tab, taskbar grouping and icons on GNOME-X11 and KDE-X11; from a second x11rb connection
  set `WM_TRANSIENT_FOR` on one and verify the dialog attaches and winit does not fight the
  property. **Settles:** winit as the X11 backend, and the sidecar as a real escape hatch.
  Failure → the X11 backend is written natively on x11rb behind the same §3 seam.
- **S2 — winit-Wayland identity** *(before the Wayland-backend milestone; not a P2b blocker).*
  Verify per-window app_id via `with_name` and taskbar matching against a generated `.desktop`
  file; determine whether current winit exposes toplevel parent (`xdg_toplevel.set_parent`)
  and xdg-toplevel-icon; verify `CustomCursor` on Wayland. **Settles:** winit vs
  smithay-client-toolkit for the Wayland backend. The transient-for and popup requirements
  make sctk the likely answer; do not pre-build the winit Wayland path beyond what falls out
  for free.
- **S3 — VA-API → wgpu upload path** *(pre-P5; already mandated by audit finding 6).*
  Standalone binary: decode one H.264 stream via VA-API; path A imports the surface as DMA-BUF
  through `wgpu_hal` Vulkan (`VK_EXT_external_memory_dma_buf` + DRM modifier negotiation);
  path B copies via `vaDeriveImage`/`vaMapBuffer` → `queue.write_texture`; both feed the same
  NV12→RGB shader. Run on the actual target GPU. **Settles:** only the upload path inside
  `GpuPresenter` — wgpu-at-P5 stands either way, because path B is strictly better than CPU
  color conversion. Also produces the **NVIDIA decision** (NVDEC via ffmpeg, or accept SW
  decode there) — that must come out of this spike, not be deferred again.
- **Open — presented-time fidelity.** The CPU presenter's `presented_us` is
  "`present()` returned", not scanout. Fine for FrameAck flow control; the latency harness
  (P6) should note the bias. The GPU presenter can use real presentation feedback at P5.
- **Open — menus/tooltips/popups.** The agent does not yet stream unowned popup windows
  (untitled windows are filtered). When it does, X11 wants override-redirect (winit exposes
  it) and Wayland wants `xdg_popup` + positioner (winit does not). This lands after P5 and is
  the strongest input to S2's outcome; it is deliberately *not* solved here.

## 8. Out of scope, on purpose

The `.desktop`/icon-cache desktop-integration subsystem (audit 26), multi-monitor layout
mapping beyond the identity mapping, fractional-scale rendering (frames present 1:1 physical
in v1; `DisplayLayout` already carries the rationals for the real fix), clipboard/audio, and
the repo rename. Each is real work; none changes the seam defined here.
