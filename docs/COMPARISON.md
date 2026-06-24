# Comparison

**English** | [한국어](COMPARISON.ko.md)

> **Status: pre-alpha.** This compares oxrdp's *intended* design against existing options.
> oxrdp does not yet function; treat this as a statement of goals, not benchmarks.

## Why not just keep using FreeRDP?

winpodx currently drives `xfreerdp3` (FreeRDP 3.x) for RemoteApp / RAIL. It works, but the
dependency is the source of recurring pain that motivated oxrdp:

- RAIL window-mapping correctness varies by FreeRDP point-release (z-order, missing
  windows, popups, taskbar / `WM_CLASS` mapping).
- A large C codebase sits in the critical path; crashes and undefined behavior reach users.
- GFX / multi-monitor / HiDPI / clipboard / audio each carry version-specific quirks.

oxrdp's thesis is to **own the protocol stack in safe Rust**, designed around RAIL and
native Linux window integration from day one.

## oxrdp vs. the alternatives

| | **oxrdp** | **FreeRDP 3.x** | **IronRDP** | **winapps** |
|---|---|---|---|---|
| Language / safety | Rust, memory-safe core | C | Rust, memory-safe | Shell + FreeRDP |
| Approach | From-scratch stack | Mature C stack | Rust RDP library | Wrapper around FreeRDP RemoteApp |
| RAIL / RemoteApp focus | Primary design goal | Supported, quirky | Partial | Via FreeRDP |
| Linux native-window integration | Built-in (X11 + Wayland) | RAIL on X11 | Not its focus | Via FreeRDP |
| Rendering | `wgpu` GPU + VA-API decode | Software / GDI paths | App-provided | Via FreeRDP |
| Architecture | sans-io, fuzzable core | Monolithic | sans-io | N/A |
| Relationship | winpodx's engine | What oxrdp replaces | Design inspiration | Independent predecessor |

## On IronRDP

[IronRDP](https://github.com/Devolutions/IronRDP) is a capable, memory-safe Rust RDP stack
and its sans-io design directly informed oxrdp's architecture. oxrdp nonetheless implements
its own stack from scratch — a deliberate, eyes-open decision to keep full control over the
RAIL semantics and Linux-native window integration that are oxrdp's whole reason to exist,
rather than fitting them on top of an external stack. No IronRDP source is copied.

## On winapps

[winapps](https://github.com/winapps-org/winapps) is an independent predecessor that also
surfaces Windows apps via FreeRDP RemoteApp. It is a reference point, not a base; oxrdp
replaces the FreeRDP engine underneath rather than wrapping it.
