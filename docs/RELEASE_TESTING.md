# Release Testing

**English** | [한국어](RELEASE_TESTING.ko.md)

> **Status: planned.** oxrdp is pre-alpha; there are no releases to test yet. This page
> will hold the per-release smoke + verification checklist once the client functions. The
> outline below records the intent so the checklist can be filled in as capabilities land.

## What CI can and cannot cover

- **CI covers** the pure, sans-io core (`oxrdp-pdu`, `oxrdp-core`, `oxrdp-graphics`,
  `oxrdp-channels`, `oxrdp-rail`): unit tests and captured-trace replay, plus `fmt`,
  `clippy`, and a dependency `audit`. No RDP server is needed.
- **CI cannot cover** the shell crates that touch the network, a real windowing system,
  the GPU, and an actual Windows RDP server (`oxrdp-io`, `oxrdp-display`, `oxrdp-render`,
  `oxrdp-input`). Those must be smoke-tested by hand against a live guest before a release.

## Planned manual smoke checklist (to be filled in)

Against a real Windows RDP server (e.g. the winpodx dockur/windows guest):

- [ ] Connect + TLS handshake + logon
- [ ] Full-desktop session renders and takes keyboard/mouse input (X11)
- [ ] Single RAIL window maps with correct `WM_CLASS`, title, icon
- [ ] Multiple RAIL windows: z-order, popups, focus
- [ ] Clipboard both directions
- [ ] Audio out
- [ ] `\\tsclient` drive redirection read/write
- [ ] Multi-monitor (primary + span) and HiDPI scaling
- [ ] Wayland backend parity with X11
- [ ] VA-API hardware decode path, and `openh264` software fallback
