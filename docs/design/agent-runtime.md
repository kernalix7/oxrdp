# Agent runtime model

How `oxagent.exe` gets onto the Windows guest, which session it runs in, and how it survives
logon/logoff. This has to be settled **before** the agent grows a listener (roadmap P1d),
because the wrong session choice makes capture silently return black frames and input injection
silently do nothing.

## The constraint that decides everything

Windows isolates services in **session 0**, which has no interactive desktop. A process there:

- cannot capture a user's application windows — `Windows.Graphics.Capture` has nothing to
  capture, and any frames it does produce are of an empty desktop;
- cannot inject input into the user's session — `SendInput` targets the *calling* session's
  input queue;
- cannot see the user's window list at all — `EnumWindows` enumerates the calling session's
  window station.

So the part of the agent that captures and injects **must run inside the interactive user
session**, on the user's window station and desktop (`WinSta0\Default`).

## Chosen model: session-resident agent, launched at logon

```
Windows guest
└── interactive session (WPX-User, WinSta0\Default)
    └── oxagent.exe                ← capture + input + the oxproto listener
```

The agent is a plain user process in the interactive session, started automatically at logon.
Everything it needs — WGC, `SendInput`, window enumeration, DWM attributes — is available
directly, with no cross-session marshalling and no privilege escalation.

**Autostart:** a per-user Scheduled Task with trigger *At log on of <user>*, "Run only when user
is logged on", highest privileges **off**. Preferred over `Run` registry keys and the Startup
folder because it survives Explorer restarts, is scriptable (`schtasks.exe`), and can be set to
restart the task if it ends.

**Restart on crash:** the same task is configured to restart on failure (up to N times), so a
crash in the capture path recovers without user action.

### Why not a Windows service

A service (session 0) would need to spawn a per-session worker via
`WTSQueryUserToken` + `CreateProcessAsUser`, which requires `SeTcbPrivilege`, and would still put
all real work in the same interactive process we can simply start directly. The service model
buys "runs before logon" — which is worthless here, because there is nothing to capture before
logon. It costs an elevated component and a much larger attack surface on a process that already
accepts network connections. Rejected.

### Consequences to accept

- **No pre-logon operation.** If nobody is logged in on the guest, there is no agent and no
  windows. The launcher (winpodx) already provisions an auto-logon user, so this matches how the
  guest is used.
- **Session switch / lock.** On lock or fast-user-switch the session's desktop becomes the
  secure desktop; capture of the user's windows stops until unlock. The agent must report this
  as an error (`CAPTURE_FAILED`) rather than streaming black frames.
- **UAC and elevated windows.** A non-elevated agent cannot capture or inject into an elevated
  ("Run as administrator") window, and cannot see the secure desktop at all. This is a Windows
  security boundary and must be surfaced to the user as "this window can't be shared", not
  worked around.

## Deployment

The agent is a single self-contained `oxagent.exe`, cross-compiled from Linux (see
`docs/HANDOFF.md` §5). Deployment into the guest:

1. Copy `oxagent.exe` and its config into the guest (the launcher already has a file channel to
   the guest).
2. Register the logon Scheduled Task on first install.
3. Provision the **auth token** and the agent's **certificate** at the same time, writing the
   token to a file readable only by the agent's user; the client receives the matching token and
   the certificate pin from the launcher. The token never appears in a command line — argv is
   readable by every process on the machine.

Upgrades replace the binary and restart the task; the protocol's version range
(`ClientHello.version_min/max`) covers a client and agent that are briefly out of step.

## Process structure

```
oxagent.exe
├── main thread            — Scheduled-Task lifecycle, config, supervision
├── listener task (tokio)  — TLS accept, handshake/auth, then per-session tasks
├── window-event thread    — window add/remove/move/title/z-order → protocol events
└── capture thread(s)      — one per captured window
```

**Threading rule.** WGC and D3D11 are COM: the capture threads call `RoInitialize` with
`RO_INIT_MULTITHREADED` and use `Direct3D11CaptureFramePool::CreateFreeThreaded`, so no thread
needs a `DispatcherQueue` or a message pump. Captured frames cross into the tokio world through a
bounded channel — bounded so that a slow network applies back-pressure to capture instead of
growing an unbounded queue, which is the same discipline the protocol's frame-ack budget applies
end to end.

**Window events.** Polling `EnumWindows` is the bring-up path; the production path is a
`SetWinEventHook` for `EVENT_OBJECT_CREATE/DESTROY/LOCATIONCHANGE/NAMECHANGE` and
`EVENT_SYSTEM_FOREGROUND`, which requires a message pump — so the window-event thread is the one
thread that runs one.

## Open items

- Whether one process per session is enough, or the agent should also handle a second logged-in
  user (multi-session guests are out of scope for v0).
- Certificate rotation policy.
- Behaviour when the guest has no GPU: `create_d3d_device` already falls back to WARP, but the
  frame rate that yields on the target guest is unmeasured.
