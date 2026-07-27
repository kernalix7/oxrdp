# Security Policy

**English** | [한국어](docs/SECURITY.ko.md)

## Supported Versions

| Version | Supported |
|---------|-----------|
| Latest  | Yes       |

oxrdp is pre-alpha; only the latest `main` is supported. Once releases begin, this table will
track supported release lines.

## Reporting a Vulnerability

Please report security vulnerabilities through GitHub Security Advisories:

**[Report a vulnerability](https://github.com/kernalix7/oxrdp/security/advisories/new)**

**Do NOT open a public issue for security vulnerabilities.**

### What to Include

- **Description** of the vulnerability and its impact
- **Steps to reproduce**
- **Affected component**: the Windows agent (`oxagent`), the Linux client (`oxclient`), the
  protocol (`oxproto` / `oxtransport`), or the shelved RDP client
- **Environment**: OS and version on both ends, oxrdp version/commit, display server
  (X11/Wayland), GPU / VA-API driver, Windows build

## Response Timeline

| Step | Timeframe |
|------|-----------|
| Acknowledgment | Within 48 hours |
| Assessment | Within 7 days |
| Fix | Within 30 days |

## Threat model

oxrdp streams individual Windows application windows from a Windows guest **agent** to a Linux
**client** over a custom protocol. The security posture is dominated by one fact:

> **The agent is a server that shares screen content and injects synthetic input into a
> logged-in interactive Windows desktop.** Anyone who can reach it and authenticate can see and
> control that session.

This inverts the posture of the project's earlier RDP-client design, where the remote end was
the powerful party and the client only had to defend its parser. Both directions now matter:

1. **The agent must not serve an unauthenticated peer.** Screen capture plus input injection is
   equivalent to remote control of the user's Windows session.
2. **Both ends parse untrusted input.** The agent decodes whatever reaches its socket — before
   authentication for the handshake message — and the client decodes whatever a (possibly
   compromised) guest sends.
3. **Loopback is not a trust boundary.** "It only listens on 127.0.0.1" does not exclude other
   local users or processes on the host.

### Required controls

These are protocol-level requirements, specified in
[`docs/design/OXPROTO.md`](docs/design/OXPROTO.md) §2 and §7:

- **Mandatory transport encryption.** The agent presents a self-signed certificate; the client
  pins its SPKI hash, provisioned out of band by whatever launches both ends. Trust-on-first-use
  *without* pinning is not acceptable for a peer that can inject input.
- **Authentication before anything else.** `ClientHello` carries a shared token, compared in
  constant time. No other message type is processed until it passes, and a failed handshake
  allocates no per-session state.
- **Bind explicitly.** The agent binds a specific interface, never `0.0.0.0` by default.
- **Bounded parsing.** Every message type has a maximum size, enforced from the chunk header
  before any buffer grows; reassembly buffers grow with arriving data, never to a declared
  length. See `crates/oxproto/src/envelope.rs`.

### Status

Implementation status is tracked honestly here rather than implied:

- Protocol-level auth token and size limits: **implemented** in `oxproto`.
- TLS for the new protocol, certificate pinning, and constant-time token comparison in the
  agent: **not yet wired** — the agent has no listener yet. These must land with the listener
  (roadmap P1d), not after it.
- The `TofuVerifier` in `oxrdp-crypto` accepts any certificate and belongs to the shelved RDP
  client path. It must not be reused for the agent connection as-is.

## Scope

In scope:

- **Missing or bypassable authentication/encryption** on the agent's listener.
- **Memory-safety or panic-on-input defects** in `oxproto` / `oxtransport` decoding (the crates
  are `#![forbid(unsafe_code)]`; a panic reachable from peer input is still a denial of service).
- **Resource exhaustion** reachable pre-authentication (unbounded allocation, unbounded
  reassembly, connection floods).
- **Unsafe FFI defects** in `oxagent`'s Windows COM/WGC/Media Foundation code.
- **Input-injection escalation**: an authenticated client reaching windows or privileges beyond
  what was shared.
- **Path traversal or arbitrary write** in any future file-transfer/clipboard channel.
- **Credential or token exposure** in logs, argv, or on disk.

Out of scope:

- Attacks requiring physical access.
- Social engineering.
- Vulnerabilities in third-party dependencies (report upstream; we will bump the pin).
- A compromised Windows guest attacking its own user — the guest is trusted to the extent that
  the user runs applications inside it. The client still validates everything the guest sends.

## Attribution

We appreciate responsible disclosure and will credit reporters in release notes (unless
anonymity is preferred).
