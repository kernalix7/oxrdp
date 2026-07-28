# Window decorations

How a remote Windows window's frame is reconciled with the local Linux window's frame.

Status: **decided, partially implemented.** The wire flag exists, the client models it, and the
agent does not yet populate it.

## The problem, seen for real

The first end-to-end run showed a guest PowerShell window inside a native Linux window — and
two title bars. The capture came back with the Windows caption, the minimise/maximise/close
buttons, and the rounded Windows 11 corners baked into the pixels, and the local window drew its
own decorations around that.

This is not cosmetic. The project's whole point is that a Windows application should look and
behave like a Linux application. Two title bars is the single most visible way to fail that, and
whichever of them the user drags decides whether window management works at all.

## Why the obvious fix is wrong

The obvious fix is: crop the non-client area away in the agent, ship only the client area, let
the local window supply the decorations. For a plain Win32 app with a standard frame that is
exactly right.

It is wrong for a large and growing class of applications. Windows Terminal — the very window in
that first screenshot — extends its content *into* the frame with DWM, so its tab strip lives in
what the API calls non-client area. Chromium, Explorer, Office and every app using a custom
title bar do the same. Cropping those cuts off real UI. The naive rule (`WS_CAPTION` present →
crop) removes the tab strip from the app most likely to be used first.

Equally, always shipping the whole window and going borderless locally is wrong the other way:
the user gets Windows chrome that does not match their theme, does not respond to their
compositor's shortcuts, and does not tile or snap the way every other window on their desktop
does.

## The policy

Neither answer is universal, so the choice is per window and is made by the side that can see
the truth — the agent, which has the window handle:

- **Standard frame** (`window_flag::HAS_FRAME` set): the agent crops to the client area, the
  client gives the window **native Linux decorations**. The app is indistinguishable from a
  local one. Move, resize, snap, tile and shortcuts all come from the local compositor.
- **App-drawn frame** (`HAS_FRAME` clear): the agent ships the **whole window** including the
  extended frame, and the client makes its toplevel **borderless**, forwarding drags on the
  app's own title bar region as window moves. The app looks exactly as its author intended.

The flag therefore does not mean "this window has a caption". It means **"the caption is the
system's to draw, and cropping it away loses nothing"** — which is a claim about ownership, not
about the presence of a style bit. `WS_CAPTION` is necessary but nowhere near sufficient; the
agent must also establish that the app has not extended its client area into the frame.

## Consequences to hold onto

- **Geometry is client-area geometry when `HAS_FRAME` is set, and whole-window geometry when it
  is not.** Both sides must agree, or every window is offset by the caption height. `WindowGeometry`
  must be reported in the same space that frames are captured in.
- **Input coordinates follow the same space.** `PointerEvent` is window-relative (`OXPROTO.md`
  §13); "the window" here means whatever the frames are cropped to. Getting this wrong shifts
  every click by the caption height — and it will look like a capture bug, not an input bug.
- **The flag can change while a window is open.** Apps switch between DWM-extended and standard
  frames (entering full screen is the common case). A change has to re-announce geometry, since
  the coordinate space just moved.
- **Wayland cannot be relied on for placement** either way — see the geometry policy in
  `client-display.md`. Decoration choice is orthogonal to that and applies on both backends.

## Open, deliberately

Whether a single bit can carry this. If it turns out that "system-owned caption" and "safe to
crop" come apart in practice — a window with a standard frame whose *content* nonetheless needs
the frame pixels — the wire needs an explicit crop rectangle instead of a boolean, and the agent
should send the client-area rect alongside the window rect. That is an additive change to
`WindowOpened` and should be taken as soon as a real app demonstrates the case, rather than
being designed for speculatively now.
