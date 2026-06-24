# Security Policy

**English** | [한국어](docs/SECURITY.ko.md)

## Supported Versions

| Version | Supported |
|---------|-----------|
| Latest  | Yes       |

oxrdp is pre-alpha; only the latest `main` is supported. Once releases begin, this table
will track supported release lines.

## Reporting a Vulnerability

Please report security vulnerabilities through GitHub Security Advisories:

**[Report a vulnerability](https://github.com/kernalix7/oxrdp/security/advisories/new)**

**Do NOT open a public issue for security vulnerabilities.**

### What to Include

- **Description**: A clear description of the vulnerability
- **Steps to Reproduce**: Detailed steps to reproduce the issue
- **Impact**: The potential impact of the vulnerability
- **Affected Components**: Which crate(s) or module(s) are affected
- **Environment**:
  - Operating System and version
  - oxrdp version / commit
  - Display server (X11 / Wayland) and desktop environment
  - GPU / VA-API driver (for decode-path issues)
  - The RDP server / Windows version on the other end

## Response Timeline

| Step | Timeframe |
|------|-----------|
| Acknowledgment | Within 48 hours |
| Assessment | Within 7 days |
| Fix | Within 30 days |

## Threat Model

oxrdp is an RDP **client**. The server it connects to is, from the client's point of
view, **untrusted network input**. Every byte that crosses the wire — connection-sequence
PDUs, capability sets, virtual-channel data, GFX/RemoteFX/bitmap codec bitstreams, RAIL
window metadata, clipboard and drive-redirection payloads — is parsed by oxrdp and must be
treated as potentially hostile, even when the server is "your own" Windows guest (a guest
can be compromised through ordinary Windows-side activity).

Memory safety is the core defense: the protocol stack, RAIL state machine, and codec
framing are written in safe Rust, so a malformed PDU yields a typed parse error, not
memory corruption.

## Scope

In scope for security reports:

- **Memory-safety defects** in PDU / channel / RAIL parsing reachable from server input
  (panics that should be recoverable errors, any `unsafe` that can be driven to UB).
- **Codec-decode safety** — out-of-bounds or UB in the H.264 / RemoteFX / bitmap decode
  paths when fed a hostile bitstream (including the FFI boundary to `openh264` / VA-API).
- **TLS / certificate validation bypass** — accepting a server identity that the
  configured trust policy (TOFU / pinned / system-CA) should reject.
- **Credential exposure** — passwords or tokens written to logs, disk, argv, or env in
  a way that leaks them.
- **Command / argument injection** in `oxrdp-cli` or the winpodx IPC control channel
  (server- or guest-derived strings reaching a shell or argv).
- **Path traversal** in drive redirection (`\\tsclient`) — a server escaping the shared
  root to read or write arbitrary host paths.

## Out of Scope

- Attacks requiring physical access to the machine.
- Social engineering attacks.
- Vulnerabilities in third-party dependencies (report these to the upstream project;
  we will bump the pin once upstream ships a fix).
- Denial of service from a server you control deliberately misbehaving (resource caps
  are hardening, not a trust boundary, in the winpodx single-tenant model).

## Security Best Practices

- **Safe-Rust core**: no `unsafe` in the protocol / RAIL / channel logic without a
  reviewed `// SAFETY:` justification; FFI to codec libraries is isolated and bounds-checked.
- **Untrusted-server posture**: all server input is size-capped and validated before it
  can allocate, write to disk, or be interpolated into a command line.
- **No secrets in code or git**: credentials, tokens, and keys are never committed.
- **Explicit certificate trust**: TLS certificate handling is explicit (TOFU / pin /
  system trust), never silently accept-all in default builds.
- **Argv-only subprocess**: no `shell=true`-style invocation of server- or guest-derived
  strings.

## Attribution

We appreciate responsible disclosure and will credit reporters in release notes (unless
anonymity is preferred).
