# Changelog

**English** | [한국어](docs/CHANGELOG.ko.md)

All notable changes to oxrdp are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims to follow
[Semantic Versioning](https://semver.org/) once releases begin.

## [Unreleased]

### Highlights

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

[Unreleased]: https://github.com/kernalix7/oxrdp/commits/main
