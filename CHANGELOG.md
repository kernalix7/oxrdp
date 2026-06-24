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

[Unreleased]: https://github.com/kernalix7/oxrdp/commits/main
