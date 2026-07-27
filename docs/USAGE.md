# Usage

**English** | [한국어](USAGE.ko.md)

> [!IMPORTANT]
> **Superseded (2026-07-02).** This document describes the original design, in which oxrdp was
> an RDP *client*. The project has since pivoted to **replacing RDP** with a custom low-latency
> protocol (a Windows guest agent streaming individual app windows to a Linux client). Current
> state: [`docs/HANDOFF.md`](HANDOFF.md) · protocol: [`docs/design/OXPROTO.md`](design/OXPROTO.md)
> · gaps: [`docs/design/AUDIT-2026-07.md`](design/AUDIT-2026-07.md). Kept for the parts that
> still apply (the client shells, the codec base) and for history.


> **Status: planned — not yet usable.** oxrdp is pre-alpha; the client does not run yet.
> This page is a placeholder. It will document the `oxrdp-cli` flag surface and the
> library `Session` API once they exist and work. Until then it deliberately describes no
> commands, to avoid implying functionality that is not there.

## Intended shape

- **Library** (`oxrdp` crate): a high-level `Session` API that winpodx (and others) drive.
- **Thin binary** (`oxrdp-cli`): launches a session — a full desktop, or a RAIL /
  RemoteApp window — against an RDP server, wiring the library to a display backend.
- **winpodx integration**: for v0, winpodx spawns `oxrdp-cli` and controls it over a
  socket/JSON channel (see [ARCHITECTURE.md §6](ARCHITECTURE.md#6-winpodx-integration-shape)).

The concrete flags and API will be documented here at the M2–M5 milestones.
